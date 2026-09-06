/// Frozen append-only status code returned by the C ABI.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[repr(i32)]
pub enum ZeErrorCode {
    /// Success.
    ZeOk = 0,
    /// A pointer, size, enum value, or request field was invalid.
    ZeErrInvalidArgument = 1,
    /// The handle value was never valid.
    ZeErrInvalidHandle = 2,
    /// The generation-tagged handle is closed or stale.
    ZeErrClosed = 3,
    /// The store is currently closing.
    ZeErrClosing = 4,
    /// A prior caught panic poisoned the handle.
    ZeErrPoisoned = 5,
    /// A Rust panic was caught at the ABI boundary.
    ZeErrPanic = 6,
    /// Another FFI writer call owns this handle's writer slot.
    ZeErrBusy = 7,
    /// Another process or handle owns the store's kernel writer lock.
    ZeErrStoreBusy = 8,
    /// An operating-system I/O operation failed.
    ZeErrIo = 9,
    /// Persisted data failed validation.
    ZeErrCorrupt = 10,
    /// The requested mode or operation is unsupported.
    ZeErrUnsupported = 11,
    /// Cooperative cancellation stopped the operation.
    ZeErrCancelled = 12,
    /// A monotonic deadline expired.
    ZeErrTimeout = 13,
    /// A checked allocation could not be reserved.
    ZeErrOutOfMemory = 14,
    /// A configured memory, disk, or work budget was exceeded.
    ZeErrBudgetExceeded = 15,
    /// A mutation batch or active segment was empty.
    ZeErrEmptyBatch = 16,
    /// A document revision would move backward.
    ZeErrStaleRevision = 17,
    /// Vector dimensions disagreed.
    ZeErrDimensionMismatch = 18,
    /// A requested object was not found.
    ZeErrNotFound = 19,
    /// A synchronization primitive was poisoned or unavailable.
    ZeErrSynchronization = 20,
    /// The handle's access mode forbids the operation.
    ZeErrAccessMode = 21,
    /// An invariant failed without a more specific public classification.
    ZeErrInternal = 22,
    /// The caller's declared embedding epoch differs from the store identity.
    ZeErrEpochMismatch = 23,
    /// The store requires an embedding epoch declaration, but none was supplied.
    ZeErrEpochUndeclared = 24,
    /// An embedding epoch was declared for a store that has no stamped identity.
    ZeErrEpochUnstamped = 25,
    /// The target epoch does not contain exactly the published live revisions.
    ZeErrEpochIncomplete = 26,
    /// The published epoch cannot be dropped while queries name it.
    ZeErrEpochPublished = 27,
    /// An epoch transition was attempted before active writes were sealed.
    ZeErrUnsealedWrites = 28,
    /// A text model bundle was absent, malformed, or corrupt.
    ZeErrBundle = 29,
    /// The configured text model runtime failed.
    ZeErrModel = 30,
    /// A bounded text pipeline stage failed.
    ZeErrPipeline = 31,
    /// A scan continuation no longer names the current store generation.
    ZeErrScanStale = 32,
    /// A declared namespace schema differs from the persisted schema.
    ZeErrSchemaMismatch = 33,
    /// The requested operation requires a vector space.
    ZeErrNoVectorSpace = 34,
}

/// Opaque generation-tagged store handle.
pub type ZeHandle = u64;

/// Opaque generation-tagged cooperative-cancellation handle.
pub type ZeCancelToken = u64;

/// Frozen ABI version returned by `ze_abi_version`.
pub const ZE_ABI_VERSION: u32 = 1;

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

/// One caller-owned namespace attribute declaration.
#[derive(Clone, Copy)]
#[repr(C)]
pub struct ZeAttributeDefinition {
    /// Schema-local identifier; zero is reserved for the `ts` column.
    pub attribute_id: u32,
    /// Caller-owned UTF-8 attribute name.
    pub name: *const u8,
    /// Number of attribute-name bytes.
    pub name_len: usize,
    /// `1` U64, `2` I64, `3` F64, `4` Bool, `5` dictionary string, or `6` raw string.
    pub attribute_type: i32,
    /// One when the attribute is nullable.
    pub nullable: u32,
}

/// Schema and vector-space identity declared for one namespace.
#[derive(Clone, Copy)]
#[repr(C)]
pub struct ZeNamespaceSpec {
    /// Caller-provided `sizeof(ZeNamespaceSpec)`.
    pub abi_size: u32,
    /// Must be zero in ABI v1.
    pub abi_reserved: u32,
    /// Caller-owned attribute definitions.
    pub attributes: *const ZeAttributeDefinition,
    /// Number of attribute definitions.
    pub attribute_count: usize,
    /// One for a vector namespace, zero for a record-only namespace.
    pub has_vector_space: u32,
    /// Vector dimensions, or zero or one for a record-only namespace.
    pub dimensions: u32,
    /// `0` for none or `1` for unit-L2 normalization.
    pub normalization: i32,
    /// Optional caller-owned epoch; null selects the canonical namespace epoch.
    pub epoch: *const ZeEpochRequest,
}

/// Opens or idempotently creates one namespace under a database root.
#[derive(Clone, Copy)]
#[repr(C)]
pub struct ZeNamespaceOpenRequest {
    /// Caller-provided `sizeof(ZeNamespaceOpenRequest)`.
    pub abi_size: u32,
    /// Must be zero in ABI v1.
    pub abi_reserved: u32,
    /// Caller-owned UTF-8 database-root path.
    pub root: *const u8,
    /// Number of root-path bytes.
    pub root_len: usize,
    /// Caller-owned namespace-name bytes.
    pub name: *const u8,
    /// Number of namespace-name bytes.
    pub name_len: usize,
    /// Existing store-open settings; its path fields are ignored.
    pub open: ZeOpenRequest,
    /// Required namespace schema and vector-space declaration.
    pub spec: *const ZeNamespaceSpec,
}

/// Requests namespace discovery immediately below one database root.
#[derive(Clone, Copy)]
#[repr(C)]
pub struct ZeNamespaceListRequest {
    /// Caller-provided `sizeof(ZeNamespaceListRequest)`.
    pub abi_size: u32,
    /// Must be zero in ABI v1.
    pub abi_reserved: u32,
    /// Caller-owned UTF-8 database-root path.
    pub root: *const u8,
    /// Number of root-path bytes.
    pub root_len: usize,
}

/// One callee-owned namespace name.
#[derive(Clone, Copy)]
#[repr(C)]
pub struct ZeNamespaceEntry {
    /// Name bytes owned by the containing result arena.
    pub name: *const u8,
    /// Number of name bytes.
    pub name_len: usize,
}

/// Callee-owned namespace list; release with `ze_namespace_list_result_free`.
#[derive(Clone, Copy)]
#[repr(C)]
pub struct ZeNamespaceListResult {
    /// Caller-provided structure size.
    pub abi_size: u32,
    /// Caller sets zero; callee returns an opaque allocation generation.
    pub abi_reserved: u32,
    /// Callee-owned entry array, or null when `entry_count` is zero.
    pub entries: *mut ZeNamespaceEntry,
    /// Number of initialized entries.
    pub entry_count: usize,
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
    /// Caller-owned UTF-8 document text for the lexical index, or null.
    pub text: *const u8,
    /// Number of `text` bytes; zero means the document carries no text.
    pub text_len: usize,
}

/// One typed schema attribute value supplied to an upsert.
#[derive(Clone, Copy)]
#[repr(C)]
pub struct ZeAttributeValue {
    /// Schema-local identifier; zero is reserved for the `ts` column.
    pub attribute_id: u32,
    /// `0` null, `1` U64, `2` I64, `3` F64, `4` Bool, or `5` string.
    pub value_type: i32,
    /// Unsigned-integer payload when `value_type` is one.
    pub u64_value: u64,
    /// Signed-integer payload when `value_type` is two.
    pub i64_value: i64,
    /// Floating-point payload when `value_type` is three.
    pub f64_value: f64,
    /// Boolean payload when `value_type` is four.
    pub bool_value: u32,
    /// Caller-owned UTF-8 bytes when `value_type` is five.
    pub string_value: *const u8,
    /// Number of string bytes.
    pub string_len: usize,
}

/// One ingest document plus its typed schema attributes.
#[derive(Clone, Copy)]
#[repr(C)]
pub struct ZeUpsertDocument {
    /// Caller-provided `sizeof(ZeUpsertDocument)`.
    pub abi_size: u32,
    /// Must be zero in ABI v1.
    pub abi_reserved: u32,
    /// Existing v1 ingest document, embedded by value.
    pub document: ZeIngestDocument,
    /// Caller-owned attribute-value array.
    pub attributes: *const ZeAttributeValue,
    /// Number of attribute values.
    pub attribute_count: usize,
}

/// Atomic document upsert request with typed schema attributes.
#[derive(Clone, Copy)]
#[repr(C)]
pub struct ZeUpsertRequest {
    /// Caller-provided `sizeof(ZeUpsertRequest)`.
    pub abi_size: u32,
    /// Must be zero in ABI v1.
    pub abi_reserved: u32,
    /// Caller-owned `ZeUpsertDocument` array.
    pub documents: *const ZeUpsertDocument,
    /// Number of document records.
    pub document_count: usize,
    /// Vector dimension for every record.
    pub dimension: usize,
}

/// Requests documents by stable id in caller order.
#[derive(Clone, Copy)]
#[repr(C)]
pub struct ZeGetRequest {
    /// Caller-provided `sizeof(ZeGetRequest)`.
    pub abi_size: u32,
    /// Must be zero in ABI v1.
    pub abi_reserved: u32,
    /// Caller-owned document-id array.
    pub ids: *const ZeDocId,
    /// Number of requested ids; must be nonzero.
    pub id_count: usize,
    /// One to return full-precision vectors.
    pub include_vector: u32,
    /// One to return stored UTF-8 text.
    pub include_text: u32,
    /// One to return opaque metadata bytes.
    pub include_metadata: u32,
    /// One to return typed schema attributes.
    pub include_attributes: u32,
}

/// One read-side document slot corresponding to one requested id.
#[derive(Clone, Copy)]
#[repr(C)]
pub struct ZeStoredDocument {
    /// One when the requested document is live, zero for a miss or tombstone.
    pub has_document: u32,
    /// Requested stable id, including for a missing document.
    pub doc_id: ZeDocId,
    /// Live revision, or zero when `has_document` is zero.
    pub revision: u64,
    /// Canonical timestamp, or zero when `has_document` is zero.
    pub timestamp: i64,
    /// Full-precision vector owned by the result arena.
    pub vector: *const f32,
    /// Scalar count in `vector`.
    pub vector_len: usize,
    /// Stored UTF-8 bytes owned by the result arena.
    pub text: *const u8,
    /// Number of `text` bytes.
    pub text_len: usize,
    /// Opaque metadata bytes owned by the result arena.
    pub metadata: *const u8,
    /// Number of metadata bytes.
    pub metadata_len: usize,
    /// Typed schema values owned by the result arena.
    pub attributes: *const ZeAttributeValue,
    /// Number of entries in `attributes`.
    pub attribute_count: usize,
}

/// Callee-owned documents returned by `ze_get`.
#[derive(Clone, Copy)]
#[repr(C)]
pub struct ZeGetResult {
    /// Caller-provided `sizeof(ZeGetResult)`.
    pub abi_size: u32,
    /// Caller sets zero; callee returns an opaque allocation generation.
    pub abi_reserved: u32,
    /// One arena-owned entry per requested id.
    pub documents: *mut ZeStoredDocument,
    /// Number of entries in `documents`; always the request's `id_count`.
    pub document_count: usize,
    /// Number of entries whose `has_document` is zero.
    pub missing_count: usize,
    /// Store generation pinned for the entire read.
    pub generation: u64,
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

/// Text store plus immutable model-bundle open request.
#[derive(Clone, Copy)]
#[repr(C)]
pub struct ZeTextOpenRequest {
    /// Caller-provided structure size.
    pub abi_size: u32,
    /// Must be zero.
    pub abi_reserved: u32,
    /// Existing core store-open fields.
    pub store: ZeOpenRequest,
    /// Caller-owned UTF-8 `.zem` path.
    pub bundle_path: *const u8,
    /// Number of bundle-path bytes.
    pub bundle_path_len: usize,
}

/// One caller-owned text document.
#[derive(Clone, Copy)]
#[repr(C)]
pub struct ZeTextDocument {
    /// Caller-provided structure size.
    pub abi_size: u32,
    /// Must be zero.
    pub abi_reserved: u32,
    /// Stable caller document id; only the high 96-bit text namespace fits.
    pub doc_id: ZeDocId,
    /// Monotonic document revision.
    pub revision: u64,
    /// Caller-owned UTF-8 text.
    pub text: *const u8,
    /// Number of text bytes.
    pub text_len: usize,
}

/// Bounded text-ingest request.
#[derive(Clone, Copy)]
#[repr(C)]
pub struct ZeTextIngestRequest {
    /// Caller-provided structure size.
    pub abi_size: u32,
    /// Must be zero.
    pub abi_reserved: u32,
    /// Caller-owned document array.
    pub documents: *const ZeTextDocument,
    /// Number of document records.
    pub document_count: usize,
    /// Model rows evaluated in one MLX call.
    pub embed_batch_size: usize,
    /// Documents between immutable seal boundaries.
    pub seal_every: usize,
    /// Bounded tokenized-batch channel capacity.
    pub channel_capacity: usize,
}

/// Text query leg selection.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[repr(i32)]
pub enum ZeTextLegs {
    /// Dense model retrieval.
    Dense = 0,
    /// Pinned-analyzer lexical retrieval.
    Lexical = 1,
    /// Bundle-alpha hybrid retrieval.
    Hybrid = 2,
}

/// Single-pass text query request.
#[derive(Clone, Copy)]
#[repr(C)]
pub struct ZeTextQueryRequest {
    /// Caller-provided structure size.
    pub abi_size: u32,
    /// Must be zero.
    pub abi_reserved: u32,
    /// Caller-owned UTF-8 query text without a model prefix.
    pub text: *const u8,
    /// Number of query bytes.
    pub text_len: usize,
    /// Requested hit count.
    pub k: usize,
    /// Dense, lexical, or hybrid.
    pub legs: i32,
    /// Must be zero.
    pub reserved: u32,
}

/// One callee-owned text query hit.
#[derive(Clone, Copy)]
#[repr(C)]
pub struct ZeTextQueryHit {
    /// Original caller document id.
    pub doc_id: ZeDocId,
    /// Document revision.
    pub revision: u64,
    /// Chunk index.
    pub chunk: u32,
    /// Must be zero.
    pub reserved: u32,
    /// Callee-owned UTF-8 hit text.
    pub text: *mut u8,
    /// Number of hit-text bytes.
    pub text_len: usize,
    /// Larger-is-better result score.
    pub score: f64,
    /// One when `vector_squared_l2` is present.
    pub has_vector_score: u32,
    /// One when `lexical_bm25` is present.
    pub has_lexical_score: u32,
    /// Exact dense squared L2 distance.
    pub vector_squared_l2: f64,
    /// Exact lexical BM25 score.
    pub lexical_bm25: f64,
}

/// Callee-owned text result; release with `ze_text_query_result_free`.
#[derive(Clone, Copy)]
#[repr(C)]
pub struct ZeTextQueryResult {
    /// Caller-provided structure size.
    pub abi_size: u32,
    /// Callee-owned allocation generation; caller initializes zero.
    pub abi_reserved: u32,
    /// Callee-owned hit array.
    pub hits: *mut ZeTextQueryHit,
    /// Number of initialized hits.
    pub hit_count: usize,
    /// Full bundle pair embedding epoch.
    pub embedding_epoch: u64,
    /// Pinned store tokenizer epoch.
    pub tokenizer_epoch: u64,
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
    /// One when `tier` carries an explicit preference; zero expresses no
    /// preference, which is distinct from explicitly choosing `tier` zero.
    pub has_tier: u32,
    /// `0` automatic, `1` exact, `2` scan, or `3` explicit graph.
    pub tier: i32,
    /// `0` SIFT-class or `1` angular graph defaults.
    pub graph_profile: i32,
    /// Must be zero.
    pub reserved: u32,
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
    /// Caller sets zero; callee returns an opaque allocation generation.
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

/// One encoder tower of an embedding epoch declaration. Every pointer is
/// caller-owned UTF-8 or opaque bytes that need only outlive the call.
#[derive(Clone, Copy)]
#[repr(C)]
pub struct ZeEmbeddingTower {
    /// Host-selected model identifier (UTF-8).
    pub model_id: *const u8,
    /// Number of `model_id` bytes.
    pub model_id_len: usize,
    /// Host-selected model version (UTF-8).
    pub model_version: *const u8,
    /// Number of `model_version` bytes.
    pub model_version_len: usize,
    /// Opaque digest of the exact model weights.
    pub weights_digest: *const u8,
    /// Number of `weights_digest` bytes.
    pub weights_digest_len: usize,
    /// Output vector dimensions.
    pub dims: u32,
    /// `0` none or `1` unit L2 normalization.
    pub normalization: i32,
    /// Exact prompt or prefix applied before inference (UTF-8), empty for none.
    pub prompt_prefix: *const u8,
    /// Number of `prompt_prefix` bytes.
    pub prompt_prefix_len: usize,
    /// Maximum input token count.
    pub max_tokens: u32,
    /// `1` Core ML, `2` MLX, or `3` CPU reference runtime.
    pub runtime: i32,
    /// `1` CPU, `2` CPU and GPU, `3` CPU and Neural Engine, or `4` all units.
    pub compute_units: i32,
    /// One when `os_build` is present.
    pub has_os_build: u32,
    /// Optional operating-system build (UTF-8).
    pub os_build: *const u8,
    /// Number of `os_build` bytes.
    pub os_build_len: usize,
}

/// Complete embedding interpretation: document tower, query tower, and the
/// alignment artifact digest.
#[derive(Clone, Copy)]
#[repr(C)]
pub struct ZeEmbeddingEpoch {
    /// Encoder used for persisted document vectors.
    pub document: ZeEmbeddingTower,
    /// Encoder used for query vectors compared with those documents.
    pub query: ZeEmbeddingTower,
    /// Opaque digest of the pairing/alignment artifact, empty for none.
    pub alignment_digest: *const u8,
    /// Number of `alignment_digest` bytes.
    pub alignment_digest_len: usize,
}

/// Caller-declared store interpretation: an embedding epoch plus a tokenizer
/// profile. The tokenizer epoch is derived from the profile because the
/// engine derives it from a tokenizer configuration, never from a raw digest.
#[derive(Clone, Copy)]
#[repr(C)]
pub struct ZeEpochRequest {
    /// Caller-provided structure size.
    pub abi_size: u32,
    /// Must be zero in ABI v1.
    pub abi_reserved: u32,
    /// Embedding interpretation.
    pub embedding: ZeEmbeddingEpoch,
    /// `0` for the general-purpose text tokenizer profile used by ingest.
    pub tokenizer_profile: i32,
    /// Must be zero.
    pub reserved: u32,
}

/// Compact epoch identity pair.
#[derive(Clone, Copy)]
#[repr(C)]
pub struct ZeEpochIdentity {
    /// Caller-provided structure size.
    pub abi_size: u32,
    /// Must be zero in ABI v1.
    pub abi_reserved: u32,
    /// Embedding identity digest.
    pub embedding_epoch: u64,
    /// Tokenizer identity digest.
    pub tokenizer_epoch: u64,
}

/// Result of one atomic published-alias transition.
#[derive(Clone, Copy)]
#[repr(C)]
pub struct ZeEpochAliasReport {
    /// Caller-provided structure size.
    pub abi_size: u32,
    /// Must be zero in ABI v1.
    pub abi_reserved: u32,
    /// Committed or unchanged generation.
    pub generation: u64,
    /// Embedding identity visible before the call.
    pub previous_embedding_epoch: u64,
    /// Tokenizer identity visible before the call.
    pub previous_tokenizer_epoch: u64,
    /// Embedding identity visible after the call.
    pub published_embedding_epoch: u64,
    /// Tokenizer identity visible after the call.
    pub published_tokenizer_epoch: u64,
    /// One when the call crossed the manifest commit point.
    pub manifest_committed: u32,
    /// Must be zero.
    pub reserved: u32,
}

/// Result of one explicit embedding-epoch drop.
#[derive(Clone, Copy)]
#[repr(C)]
pub struct ZeEpochDropReport {
    /// Caller-provided structure size.
    pub abi_size: u32,
    /// Must be zero in ABI v1.
    pub abi_reserved: u32,
    /// Generation committed without the dropped segments.
    pub generation: u64,
    /// Number of immutable segments omitted by the committed manifest.
    pub segments_dropped: u64,
    /// Exact segment-file bytes unlinked after commit.
    pub bytes_reclaimed: u64,
}

/// Structured query request. Encoding decision (task 22 phase 2): a
/// size-versioned `repr(C)` struct, the same shape as every other request in
/// this ABI, rather than a compact binary encoding. Reasons: the fields are
/// fixed-arity scalars and caller-owned buffers with no nesting, so a struct
/// is the encoding a C compiler already validates; Swift (task 23) and Python
/// (task 25) get typed field access instead of a serializer to keep in sync;
/// evolution follows the existing `abi_size` rule (append a new struct, never
/// grow this one); and the layout is pinned by an offset golden so the wire
/// shape cannot move silently.
///
/// The vector leg is present when `vector_len` is nonzero and the lexical leg
/// when `text_len` is nonzero. Both present selects hybrid fusion. Query text
/// is analyzed with the same tokenizer configuration ingest uses.
#[derive(Clone, Copy)]
#[repr(C)]
pub struct ZeQueryRequest {
    /// Caller-provided structure size.
    pub abi_size: u32,
    /// Must be zero in ABI v1.
    pub abi_reserved: u32,
    /// Caller-owned aligned f32 query vector, or null when absent.
    pub vector: *const f32,
    /// Scalar count in `vector`; zero means no vector leg.
    pub vector_len: usize,
    /// Declared vector dimension; must equal `vector_len`.
    pub dimension: usize,
    /// Caller-owned UTF-8 query text, or null when absent.
    pub text: *const u8,
    /// Number of `text` bytes; zero means no lexical leg.
    pub text_len: usize,
    /// Number of requested results.
    pub k: usize,
    /// Zero selects all detected physical performance cores.
    pub thread_budget: usize,
    /// One when `tier` carries an explicit preference. Zero expresses no
    /// preference, which is distinct from explicitly choosing `tier` zero:
    /// no preference lets the engine pick, and hybrid fusion then selects
    /// exact scoring.
    pub has_tier: u32,
    /// `0` automatic, `1` exact, `2` scan, or `3` explicit graph.
    pub tier: i32,
    /// `0` SIFT-class or `1` angular graph defaults.
    pub graph_profile: i32,
    /// Must be zero.
    pub reserved: u32,
    /// Explicit graph width, or zero for adaptive width.
    pub graph_ef: usize,
    /// Deterministic graph query-preparation seed.
    pub graph_seed: u64,
    /// One when `alpha` carries an explicit fusion weight.
    pub has_alpha: u32,
    /// One to enable the query-shape alpha rules, which are off by default
    /// (policy version 2); ignored when `has_alpha` is one.
    pub rules_enabled: u32,
    /// Explicit convex-combination alpha in `0..=1`.
    pub alpha: f64,
    /// One when `max_rounds` overrides the fusion widening cap.
    pub has_max_rounds: u32,
    /// One when the query contains a quoted phrase.
    pub quoted_phrase: u32,
    /// Fusion widening round cap; zero materializes full lists immediately.
    pub max_rounds: u64,
    /// One when token classification found an identifier.
    pub identifier_token: u32,
    /// One when `rarest_exact_document_frequency` is present.
    pub has_rarest_exact_document_frequency: u32,
    /// Lowest exact-token document frequency.
    pub rarest_exact_document_frequency: u64,
    /// Optional generation-tagged cancellation token; zero means absent.
    pub cancel_token: ZeCancelToken,
    /// Relative monotonic deadline in nanoseconds; zero means absent.
    pub deadline_ns: u64,
}

/// One callee-owned structured-query hit.
#[derive(Clone, Copy)]
#[repr(C)]
pub struct ZeQueryHit {
    /// One when document identity is present.
    pub has_document: u32,
    /// One when `revision` is present; fused hits carry identity only.
    pub has_revision: u32,
    /// Document id when `has_document` is one.
    pub doc_id: ZeDocId,
    /// Document revision when `has_revision` is one.
    pub revision: u64,
    /// Larger-is-better ranking score of the executed mode.
    pub score: f64,
    /// One when `vector_squared_l2` is present.
    pub has_vector_score: u32,
    /// One when `lexical_bm25` is present.
    pub has_lexical_score: u32,
    /// Squared L2 distance of the vector leg.
    pub vector_squared_l2: f64,
    /// BM25 score of the lexical leg.
    pub lexical_bm25: f64,
}

/// Callee-owned structured-query result; release with `ze_query_result_free`.
#[derive(Clone, Copy)]
#[repr(C)]
pub struct ZeQueryResult {
    /// Caller-provided structure size.
    pub abi_size: u32,
    /// Caller sets zero; callee returns an opaque allocation generation.
    pub abi_reserved: u32,
    /// Callee-owned hit array, or null when `hit_count` is zero.
    pub hits: *mut ZeQueryHit,
    /// Number of initialized hits.
    pub hit_count: usize,
    /// Pinned store generation queried.
    pub generation: u64,
    /// `0` vector, `1` lexical, or `2` hybrid mode executed.
    pub mode: i32,
    /// One when any candidate membership came from a non-exhaustive path.
    pub approximate: u32,
    /// One when every returned score came from full-precision rows.
    pub exact_rescore: u32,
    /// One when an execution budget fired.
    pub budget_exhausted: u32,
    /// One when the fusion fields are present.
    pub has_fusion: u32,
    /// `0` convex combination or `1` reciprocal rank fusion.
    pub fusion_method: i32,
    /// Effective fusion alpha.
    pub effective_alpha: f64,
    /// Fusion widening rounds attempted.
    pub fusion_rounds: u64,
    /// One when `embedding_epoch` is present.
    pub has_embedding_epoch: u32,
    /// One when `tokenizer_epoch` is present.
    pub has_tokenizer_epoch: u32,
    /// Embedding identity that interpreted the vector leg.
    pub embedding_epoch: u64,
    /// Tokenizer identity that interpreted the lexical leg.
    pub tokenizer_epoch: u64,
    /// Logical row-coordinate multiply-accumulates.
    pub dims_touched: u64,
    /// Row-major payload bytes read.
    pub bytes_read: u64,
    /// Lexical documents whose score was computed.
    pub docs_evaluated: u64,
    /// Lexical posting entries decoded.
    pub postings_decoded: u64,
}
