#ifndef ZEPPELIN_EMBED_H
#define ZEPPELIN_EMBED_H

#include <stddef.h>
#include <stdint.h>

/*
 Frozen ABI version returned by `ze_abi_version`.
 */
#define ZE_ABI_VERSION 1

/*
 Largest request structure accepted by ABI v1.
 */
#define ZE_ABI_MAX_STRUCT_SIZE 65536

/*
 Largest top-k request accepted by ABI v1.
 */
#define ZE_MAX_K (1 << 20)

/*
 Frozen append-only status code returned by the C ABI.
 */
enum ze_error_code
#if defined(__cplusplus) || __STDC_VERSION__ >= 202311L
  : int32_t
#endif // defined(__cplusplus) || __STDC_VERSION__ >= 202311L
 {
    /*
     Success.
     */
    ZE_OK = 0,
    /*
     A pointer, size, enum value, or request field was invalid.
     */
    ZE_ERR_INVALID_ARGUMENT = 1,
    /*
     The handle value was never valid.
     */
    ZE_ERR_INVALID_HANDLE = 2,
    /*
     The generation-tagged handle is closed or stale.
     */
    ZE_ERR_CLOSED = 3,
    /*
     The store is currently closing.
     */
    ZE_ERR_CLOSING = 4,
    /*
     A prior caught panic poisoned the handle.
     */
    ZE_ERR_POISONED = 5,
    /*
     A Rust panic was caught at the ABI boundary.
     */
    ZE_ERR_PANIC = 6,
    /*
     Another FFI writer call owns this handle's writer slot.
     */
    ZE_ERR_BUSY = 7,
    /*
     Another process or handle owns the store's kernel writer lock.
     */
    ZE_ERR_STORE_BUSY = 8,
    /*
     An operating-system I/O operation failed.
     */
    ZE_ERR_IO = 9,
    /*
     Persisted data failed validation.
     */
    ZE_ERR_CORRUPT = 10,
    /*
     The requested mode or operation is unsupported.
     */
    ZE_ERR_UNSUPPORTED = 11,
    /*
     Cooperative cancellation stopped the operation.
     */
    ZE_ERR_CANCELLED = 12,
    /*
     A monotonic deadline expired.
     */
    ZE_ERR_TIMEOUT = 13,
    /*
     A checked allocation could not be reserved.
     */
    ZE_ERR_OUT_OF_MEMORY = 14,
    /*
     A configured memory, disk, or work budget was exceeded.
     */
    ZE_ERR_BUDGET_EXCEEDED = 15,
    /*
     A mutation batch or active segment was empty.
     */
    ZE_ERR_EMPTY_BATCH = 16,
    /*
     A document revision would move backward.
     */
    ZE_ERR_STALE_REVISION = 17,
    /*
     Vector dimensions disagreed.
     */
    ZE_ERR_DIMENSION_MISMATCH = 18,
    /*
     A requested object was not found.
     */
    ZE_ERR_NOT_FOUND = 19,
    /*
     A synchronization primitive was poisoned or unavailable.
     */
    ZE_ERR_SYNCHRONIZATION = 20,
    /*
     The handle's access mode forbids the operation.
     */
    ZE_ERR_ACCESS_MODE = 21,
    /*
     An invariant failed without a more specific public classification.
     */
    ZE_ERR_INTERNAL = 22,
    /*
     The caller's declared embedding epoch differs from the store identity.
     */
    ZE_ERR_EPOCH_MISMATCH = 23,
    /*
     The store requires an embedding epoch declaration, but none was supplied.
     */
    ZE_ERR_EPOCH_UNDECLARED = 24,
    /*
     An embedding epoch was declared for a store that has no stamped identity.
     */
    ZE_ERR_EPOCH_UNSTAMPED = 25,
    /*
     The target epoch does not contain exactly the published live revisions.
     */
    ZE_ERR_EPOCH_INCOMPLETE = 26,
    /*
     The published epoch cannot be dropped while queries name it.
     */
    ZE_ERR_EPOCH_PUBLISHED = 27,
    /*
     An epoch transition was attempted before active writes were sealed.
     */
    ZE_ERR_UNSEALED_WRITES = 28,
};
#ifndef __cplusplus
#if __STDC_VERSION__ >= 202311L
typedef enum ze_error_code ze_error_code;
#else
typedef int32_t ze_error_code;
#endif // __STDC_VERSION__ >= 202311L
#endif // __cplusplus

/*
 Opens one store directory.
 */
typedef struct ZeOpenRequest {
    /*
     Caller-provided `sizeof(ZeOpenRequest)`.
     */
    uint32_t abi_size;
    /*
     Must be zero in ABI v1.
     */
    uint32_t abi_reserved;
    /*
     Caller-owned UTF-8 path bytes.
     */
    const uint8_t *path;
    /*
     Number of path bytes.
     */
    size_t path_len;
    /*
     `0` for read-write, `1` for read-only.
     */
    int32_t access_mode;
    /*
     `0` for derived, `1` for durable, `2` for attached.
     */
    int32_t durability_mode;
    /*
     `0` for none, `1` for ordered, `2` for durable.
     */
    int32_t commit_tier;
    /*
     Reader drain grace period in milliseconds.
     */
    uint64_t reader_drain_timeout_ms;
    /*
     Exact resident-owned byte ceiling.
     */
    uint64_t max_resident_bytes;
    /*
     Exact temporary byte ceiling.
     */
    uint64_t max_temp_bytes;
} ZeOpenRequest;

/*
 Opaque generation-tagged store handle.
 */
typedef uint64_t ze_handle;

/*
 One encoder tower of an embedding epoch declaration. Every pointer is
 caller-owned UTF-8 or opaque bytes that need only outlive the call.
 */
typedef struct ZeEmbeddingTower {
    /*
     Host-selected model identifier (UTF-8).
     */
    const uint8_t *model_id;
    /*
     Number of `model_id` bytes.
     */
    size_t model_id_len;
    /*
     Host-selected model version (UTF-8).
     */
    const uint8_t *model_version;
    /*
     Number of `model_version` bytes.
     */
    size_t model_version_len;
    /*
     Opaque digest of the exact model weights.
     */
    const uint8_t *weights_digest;
    /*
     Number of `weights_digest` bytes.
     */
    size_t weights_digest_len;
    /*
     Output vector dimensions.
     */
    uint32_t dims;
    /*
     `0` none or `1` unit L2 normalization.
     */
    int32_t normalization;
    /*
     Exact prompt or prefix applied before inference (UTF-8), empty for none.
     */
    const uint8_t *prompt_prefix;
    /*
     Number of `prompt_prefix` bytes.
     */
    size_t prompt_prefix_len;
    /*
     Maximum input token count.
     */
    uint32_t max_tokens;
    /*
     `1` Core ML, `2` MLX, or `3` CPU reference runtime.
     */
    int32_t runtime;
    /*
     `1` CPU, `2` CPU and GPU, `3` CPU and Neural Engine, or `4` all units.
     */
    int32_t compute_units;
    /*
     One when `os_build` is present.
     */
    uint32_t has_os_build;
    /*
     Optional operating-system build (UTF-8).
     */
    const uint8_t *os_build;
    /*
     Number of `os_build` bytes.
     */
    size_t os_build_len;
} ZeEmbeddingTower;

/*
 Complete embedding interpretation: document tower, query tower, and the
 alignment artifact digest.
 */
typedef struct ZeEmbeddingEpoch {
    /*
     Encoder used for persisted document vectors.
     */
    struct ZeEmbeddingTower document;
    /*
     Encoder used for query vectors compared with those documents.
     */
    struct ZeEmbeddingTower query;
    /*
     Opaque digest of the pairing/alignment artifact, empty for none.
     */
    const uint8_t *alignment_digest;
    /*
     Number of `alignment_digest` bytes.
     */
    size_t alignment_digest_len;
} ZeEmbeddingEpoch;

/*
 Caller-declared store interpretation: an embedding epoch plus a tokenizer
 profile. The tokenizer epoch is derived from the profile because the
 engine derives it from a tokenizer configuration, never from a raw digest.
 */
typedef struct ZeEpochRequest {
    /*
     Caller-provided structure size.
     */
    uint32_t abi_size;
    /*
     Must be zero in ABI v1.
     */
    uint32_t abi_reserved;
    /*
     Embedding interpretation.
     */
    struct ZeEmbeddingEpoch embedding;
    /*
     `0` for the general-purpose text tokenizer profile used by ingest.
     */
    int32_t tokenizer_profile;
    /*
     Must be zero.
     */
    uint32_t reserved;
} ZeEpochRequest;

/*
 Compact epoch identity pair.
 */
typedef struct ZeEpochIdentity {
    /*
     Caller-provided structure size.
     */
    uint32_t abi_size;
    /*
     Must be zero in ABI v1.
     */
    uint32_t abi_reserved;
    /*
     Embedding identity digest.
     */
    uint64_t embedding_epoch;
    /*
     Tokenizer identity digest.
     */
    uint64_t tokenizer_epoch;
} ZeEpochIdentity;

/*
 Result of one atomic published-alias transition.
 */
typedef struct ZeEpochAliasReport {
    /*
     Caller-provided structure size.
     */
    uint32_t abi_size;
    /*
     Must be zero in ABI v1.
     */
    uint32_t abi_reserved;
    /*
     Committed or unchanged generation.
     */
    uint64_t generation;
    /*
     Embedding identity visible before the call.
     */
    uint64_t previous_embedding_epoch;
    /*
     Tokenizer identity visible before the call.
     */
    uint64_t previous_tokenizer_epoch;
    /*
     Embedding identity visible after the call.
     */
    uint64_t published_embedding_epoch;
    /*
     Tokenizer identity visible after the call.
     */
    uint64_t published_tokenizer_epoch;
    /*
     One when the call crossed the manifest commit point.
     */
    uint32_t manifest_committed;
    /*
     Must be zero.
     */
    uint32_t reserved;
} ZeEpochAliasReport;

/*
 Result of one explicit embedding-epoch drop.
 */
typedef struct ZeEpochDropReport {
    /*
     Caller-provided structure size.
     */
    uint32_t abi_size;
    /*
     Must be zero in ABI v1.
     */
    uint32_t abi_reserved;
    /*
     Generation committed without the dropped segments.
     */
    uint64_t generation;
    /*
     Number of immutable segments omitted by the committed manifest.
     */
    uint64_t segments_dropped;
    /*
     Exact segment-file bytes unlinked after commit.
     */
    uint64_t bytes_reclaimed;
} ZeEpochDropReport;

/*
 Store lifecycle state report.
 */
typedef struct ZeStateReport {
    /*
     Caller-provided structure size.
     */
    uint32_t abi_size;
    /*
     Must be zero in ABI v1.
     */
    uint32_t abi_reserved;
    /*
     `0` open, `1` closing, or `2` closed.
     */
    int32_t state;
    /*
     Must be zero.
     */
    uint32_t reserved;
} ZeStateReport;

/*
 Exact store resource counters.
 */
typedef struct ZeStatsReport {
    /*
     Caller-provided structure size.
     */
    uint32_t abi_size;
    /*
     Must be zero in ABI v1.
     */
    uint32_t abi_reserved;
    /*
     Engine-owned anonymous bytes.
     */
    uint64_t resident_owned_bytes;
    /*
     Live read-only mapping bytes.
     */
    uint64_t mapped_bytes;
    /*
     Resident mapped-page bytes.
     */
    uint64_t mapped_resident_bytes;
    /*
     Immutable segment mapping bytes.
     */
    uint64_t segment_bytes;
    /*
     Active-segment allocation capacity.
     */
    uint64_t active_segment_bytes;
    /*
     Active row count.
     */
    uint64_t active_row_count;
    /*
     Active tombstone count.
     */
    uint64_t tombstone_count;
    /*
     Active tombstone bytes.
     */
    uint64_t tombstone_bytes;
    /*
     In-memory WAL bytes.
     */
    uint64_t wal_bytes;
    /*
     Reusable graph-cache bytes.
     */
    uint64_t cache_bytes;
    /*
     Live temporary bytes.
     */
    uint64_t temporary_bytes;
    /*
     Persistent query-pool registry bytes.
     */
    uint64_t query_pool_bytes;
    /*
     Retained file descriptor count.
     */
    uint64_t open_files;
    /*
     Admitted active-query count.
     */
    uint64_t active_queries;
    /*
     Externally held snapshot lease count.
     */
    uint64_t active_snapshot_leases;
    /*
     Darwin physical footprint, or zero when unavailable.
     */
    uint64_t phys_footprint;
    /*
     One when `phys_footprint` is available.
     */
    uint32_t has_phys_footprint;
    /*
     Must be zero.
     */
    uint32_t reserved;
} ZeStatsReport;

/*
 Stable 128-bit application document identifier.
 */
typedef struct ZeDocId {
    /*
     Most-significant 64 bits.
     */
    uint64_t high;
    /*
     Least-significant 64 bits.
     */
    uint64_t low;
} ZeDocId;

/*
 One document supplied to an ingest request.
 */
typedef struct ZeIngestDocument {
    /*
     Caller-provided structure size.
     */
    uint32_t abi_size;
    /*
     Must be zero in ABI v1.
     */
    uint32_t abi_reserved;
    /*
     Stable document identifier.
     */
    struct ZeDocId doc_id;
    /*
     Monotonic document revision.
     */
    uint64_t revision;
    /*
     Canonical clustering timestamp.
     */
    int64_t timestamp;
    /*
     Caller-owned aligned f32 vector.
     */
    const float *vector;
    /*
     Scalar count in `vector`.
     */
    size_t vector_len;
    /*
     Caller-owned opaque metadata bytes.
     */
    const uint8_t *metadata;
    /*
     Number of metadata bytes.
     */
    size_t metadata_len;
    /*
     Caller-owned UTF-8 document text for the lexical index, or null.
     */
    const uint8_t *text;
    /*
     Number of `text` bytes; zero means the document carries no text.
     */
    size_t text_len;
} ZeIngestDocument;

/*
 Atomic document-ingest request.
 */
typedef struct ZeIngestRequest {
    /*
     Caller-provided structure size.
     */
    uint32_t abi_size;
    /*
     Must be zero in ABI v1.
     */
    uint32_t abi_reserved;
    /*
     Caller-owned `ZeIngestDocument` array.
     */
    const struct ZeIngestDocument *documents;
    /*
     Number of document records.
     */
    size_t document_count;
    /*
     Required vector dimension for every record.
     */
    size_t dimension;
} ZeIngestRequest;

/*
 WAL and generation coordinates returned by ingest and delete.
 */
typedef struct ZeMutationReport {
    /*
     Caller-provided structure size.
     */
    uint32_t abi_size;
    /*
     Must be zero in ABI v1.
     */
    uint32_t abi_reserved;
    /*
     Last committed WAL sequence.
     */
    uint64_t sequence;
    /*
     Store generation changed by the mutation.
     */
    uint64_t generation;
} ZeMutationReport;

/*
 Atomic document-delete request.
 */
typedef struct ZeDeleteRequest {
    /*
     Caller-provided structure size.
     */
    uint32_t abi_size;
    /*
     Must be zero in ABI v1.
     */
    uint32_t abi_reserved;
    /*
     Caller-owned document-id array.
     */
    const struct ZeDocId *doc_ids;
    /*
     Number of identifiers.
     */
    size_t doc_id_count;
} ZeDeleteRequest;

/*
 Opaque generation-tagged cooperative-cancellation handle.
 */
typedef uint64_t ze_cancel_token;

/*
 Vector search request.
 */
typedef struct ZeSearchRequest {
    /*
     Caller-provided structure size.
     */
    uint32_t abi_size;
    /*
     Must be zero in ABI v1, except for the feature-gated panic probe.
     */
    uint32_t abi_reserved;
    /*
     Caller-owned aligned f32 query vector.
     */
    const float *vector;
    /*
     Scalar count in `vector`.
     */
    size_t vector_len;
    /*
     Declared vector dimension; must equal `vector_len`.
     */
    size_t dimension;
    /*
     Number of requested candidates.
     */
    size_t k;
    /*
     Zero selects all detected physical performance cores.
     */
    size_t thread_budget;
    /*
     One when `tier` carries an explicit preference; zero expresses no
     preference, which is distinct from explicitly choosing `tier` zero.
     */
    uint32_t has_tier;
    /*
     `0` automatic, `1` exact, `2` scan, or `3` explicit graph.
     */
    int32_t tier;
    /*
     `0` SIFT-class or `1` angular graph defaults.
     */
    int32_t graph_profile;
    /*
     Must be zero.
     */
    uint32_t reserved;
    /*
     Explicit graph width, or zero for adaptive width.
     */
    size_t graph_ef;
    /*
     Deterministic graph query-preparation seed.
     */
    uint64_t graph_seed;
    /*
     Optional generation-tagged cancellation token; zero means absent.
     */
    ze_cancel_token cancel_token;
    /*
     Relative monotonic deadline in nanoseconds; zero means absent.
     */
    uint64_t deadline_ns;
} ZeSearchRequest;

/*
 One callee-owned search hit.
 */
typedef struct ZeSearchHit {
    /*
     `0` for active state or `1` for an immutable segment.
     */
    uint32_t source_kind;
    /*
     Must be zero.
     */
    uint32_t reserved;
    /*
     Immutable segment id; all zero for active state.
     */
    uint8_t segment_id[16];
    /*
     Dense source-local row id.
     */
    uint32_t local_row;
    /*
     One when document identity is present.
     */
    uint32_t has_document;
    /*
     Document id when `has_document` is one.
     */
    struct ZeDocId doc_id;
    /*
     Document revision when `has_document` is one.
     */
    uint64_t revision;
    /*
     Larger-is-better exact score.
     */
    float score;
    /*
     Must be zero.
     */
    uint32_t reserved_tail;
} ZeSearchHit;

/*
 Callee-owned search result; release with `ze_search_result_free`.
 */
typedef struct ZeSearchResult {
    /*
     Caller-provided structure size.
     */
    uint32_t abi_size;
    /*
     Must be zero in ABI v1.
     */
    uint32_t abi_reserved;
    /*
     Callee-owned hit array, or null when `hit_count` is zero.
     */
    struct ZeSearchHit *hits;
    /*
     Number of initialized hits.
     */
    size_t hit_count;
    /*
     Pinned store generation searched.
     */
    uint64_t generation;
    /*
     Logical row-coordinate multiply-accumulates.
     */
    uint64_t dims_touched;
    /*
     Row-major payload bytes read.
     */
    uint64_t bytes_read;
    /*
     Query workers that scored partitions.
     */
    uint64_t threads_used;
    /*
     Sealed segments traversed through graphs.
     */
    uint64_t graph_segments_traversed;
    /*
     Complete graph validations.
     */
    uint64_t graph_validations;
    /*
     Entry-seed full scans.
     */
    uint64_t graph_entry_seed_discoveries;
    /*
     Visited-array epoch clears.
     */
    uint64_t graph_visited_epoch_clears;
    /*
     Graph estimator candidates scored.
     */
    uint64_t graph_candidates_scored;
    /*
     Graph candidates exactly rescored.
     */
    uint64_t graph_candidates_rescored;
    /*
     Segments rejected by the shared bound.
     */
    uint64_t graph_segments_pruned_by_bound;
} ZeSearchResult;

/*
 Structured query request. Encoding decision (task 22 phase 2): a
 size-versioned `repr(C)` struct, the same shape as every other request in
 this ABI, rather than a compact binary encoding. Reasons: the fields are
 fixed-arity scalars and caller-owned buffers with no nesting, so a struct
 is the encoding a C compiler already validates; Swift (task 23) and Python
 (task 25) get typed field access instead of a serializer to keep in sync;
 evolution follows the existing `abi_size` rule (append a new struct, never
 grow this one); and the layout is pinned by an offset golden so the wire
 shape cannot move silently.

 The vector leg is present when `vector_len` is nonzero and the lexical leg
 when `text_len` is nonzero. Both present selects hybrid fusion. Query text
 is analyzed with the same tokenizer configuration ingest uses.
 */
typedef struct ZeQueryRequest {
    /*
     Caller-provided structure size.
     */
    uint32_t abi_size;
    /*
     Must be zero in ABI v1.
     */
    uint32_t abi_reserved;
    /*
     Caller-owned aligned f32 query vector, or null when absent.
     */
    const float *vector;
    /*
     Scalar count in `vector`; zero means no vector leg.
     */
    size_t vector_len;
    /*
     Declared vector dimension; must equal `vector_len`.
     */
    size_t dimension;
    /*
     Caller-owned UTF-8 query text, or null when absent.
     */
    const uint8_t *text;
    /*
     Number of `text` bytes; zero means no lexical leg.
     */
    size_t text_len;
    /*
     Number of requested results.
     */
    size_t k;
    /*
     Zero selects all detected physical performance cores.
     */
    size_t thread_budget;
    /*
     One when `tier` carries an explicit preference. Zero expresses no
     preference, which is distinct from explicitly choosing `tier` zero:
     no preference lets the engine pick, and hybrid fusion then selects
     exact scoring.
     */
    uint32_t has_tier;
    /*
     `0` automatic, `1` exact, `2` scan, or `3` explicit graph.
     */
    int32_t tier;
    /*
     `0` SIFT-class or `1` angular graph defaults.
     */
    int32_t graph_profile;
    /*
     Must be zero.
     */
    uint32_t reserved;
    /*
     Explicit graph width, or zero for adaptive width.
     */
    size_t graph_ef;
    /*
     Deterministic graph query-preparation seed.
     */
    uint64_t graph_seed;
    /*
     One when `alpha` carries an explicit fusion weight.
     */
    uint32_t has_alpha;
    /*
     One to enable the query-shape alpha rules, which are off by default
     (policy version 2); ignored when `has_alpha` is one.
     */
    uint32_t rules_enabled;
    /*
     Explicit convex-combination alpha in `0..=1`.
     */
    double alpha;
    /*
     One when `max_rounds` overrides the fusion widening cap.
     */
    uint32_t has_max_rounds;
    /*
     One when the query contains a quoted phrase.
     */
    uint32_t quoted_phrase;
    /*
     Fusion widening round cap; zero materializes full lists immediately.
     */
    uint64_t max_rounds;
    /*
     One when token classification found an identifier.
     */
    uint32_t identifier_token;
    /*
     One when `rarest_exact_document_frequency` is present.
     */
    uint32_t has_rarest_exact_document_frequency;
    /*
     Lowest exact-token document frequency.
     */
    uint64_t rarest_exact_document_frequency;
    /*
     Optional generation-tagged cancellation token; zero means absent.
     */
    ze_cancel_token cancel_token;
    /*
     Relative monotonic deadline in nanoseconds; zero means absent.
     */
    uint64_t deadline_ns;
} ZeQueryRequest;

/*
 One callee-owned structured-query hit.
 */
typedef struct ZeQueryHit {
    /*
     One when document identity is present.
     */
    uint32_t has_document;
    /*
     One when `revision` is present; fused hits carry identity only.
     */
    uint32_t has_revision;
    /*
     Document id when `has_document` is one.
     */
    struct ZeDocId doc_id;
    /*
     Document revision when `has_revision` is one.
     */
    uint64_t revision;
    /*
     Larger-is-better ranking score of the executed mode.
     */
    double score;
    /*
     One when `vector_squared_l2` is present.
     */
    uint32_t has_vector_score;
    /*
     One when `lexical_bm25` is present.
     */
    uint32_t has_lexical_score;
    /*
     Squared L2 distance of the vector leg.
     */
    double vector_squared_l2;
    /*
     BM25 score of the lexical leg.
     */
    double lexical_bm25;
} ZeQueryHit;

/*
 Callee-owned structured-query result; release with `ze_query_result_free`.
 */
typedef struct ZeQueryResult {
    /*
     Caller-provided structure size.
     */
    uint32_t abi_size;
    /*
     Must be zero in ABI v1.
     */
    uint32_t abi_reserved;
    /*
     Callee-owned hit array, or null when `hit_count` is zero.
     */
    struct ZeQueryHit *hits;
    /*
     Number of initialized hits.
     */
    size_t hit_count;
    /*
     Pinned store generation queried.
     */
    uint64_t generation;
    /*
     `0` vector, `1` lexical, or `2` hybrid mode executed.
     */
    int32_t mode;
    /*
     One when any candidate membership came from a non-exhaustive path.
     */
    uint32_t approximate;
    /*
     One when every returned score came from full-precision rows.
     */
    uint32_t exact_rescore;
    /*
     One when an execution budget fired.
     */
    uint32_t budget_exhausted;
    /*
     One when the fusion fields are present.
     */
    uint32_t has_fusion;
    /*
     `0` convex combination or `1` reciprocal rank fusion.
     */
    int32_t fusion_method;
    /*
     Effective fusion alpha.
     */
    double effective_alpha;
    /*
     Fusion widening rounds attempted.
     */
    uint64_t fusion_rounds;
    /*
     One when `embedding_epoch` is present.
     */
    uint32_t has_embedding_epoch;
    /*
     One when `tokenizer_epoch` is present.
     */
    uint32_t has_tokenizer_epoch;
    /*
     Embedding identity that interpreted the vector leg.
     */
    uint64_t embedding_epoch;
    /*
     Tokenizer identity that interpreted the lexical leg.
     */
    uint64_t tokenizer_epoch;
    /*
     Logical row-coordinate multiply-accumulates.
     */
    uint64_t dims_touched;
    /*
     Row-major payload bytes read.
     */
    uint64_t bytes_read;
    /*
     Lexical documents whose score was computed.
     */
    uint64_t docs_evaluated;
    /*
     Lexical posting entries decoded.
     */
    uint64_t postings_decoded;
} ZeQueryResult;

/*
 Explicit seal request.
 */
typedef struct ZeSealRequest {
    /*
     Caller-provided structure size.
     */
    uint32_t abi_size;
    /*
     Must be zero in ABI v1.
     */
    uint32_t abi_reserved;
    /*
     Optional generation-tagged cancellation token.
     */
    ze_cancel_token cancel_token;
} ZeSealRequest;

/*
 A generation returned by seal.
 */
typedef struct ZeGenerationReport {
    /*
     Caller-provided structure size.
     */
    uint32_t abi_size;
    /*
     Must be zero in ABI v1.
     */
    uint32_t abi_reserved;
    /*
     Committed generation.
     */
    uint64_t generation;
} ZeGenerationReport;

/*
 Whole-segment timestamp partition request.
 */
typedef struct ZeDropPartitionRequest {
    /*
     Caller-provided structure size.
     */
    uint32_t abi_size;
    /*
     Must be zero in ABI v1.
     */
    uint32_t abi_reserved;
    /*
     Inclusive timestamp lower bound.
     */
    int64_t start_ts;
    /*
     Exclusive timestamp upper bound.
     */
    int64_t end_ts;
} ZeDropPartitionRequest;

/*
 Whole-segment partition mutation report.
 */
typedef struct ZePartitionReport {
    /*
     Caller-provided structure size.
     */
    uint32_t abi_size;
    /*
     Must be zero in ABI v1.
     */
    uint32_t abi_reserved;
    /*
     Committed or observed generation.
     */
    uint64_t generation;
    /*
     Number of immutable segments dropped.
     */
    uint64_t segments_dropped;
    /*
     Exact immutable bytes reclaimed.
     */
    uint64_t bytes_reclaimed;
    /*
     Number of overlapping segments retained.
     */
    uint64_t straddlers_skipped;
    /*
     One when no manifest commit or unlink occurred.
     */
    uint32_t is_no_op;
    /*
     Must be zero.
     */
    uint32_t reserved;
} ZePartitionReport;

/*
 Retention-window request.
 */
typedef struct ZeRetentionRequest {
    /*
     Caller-provided structure size.
     */
    uint32_t abi_size;
    /*
     Must be zero in ABI v1.
     */
    uint32_t abi_reserved;
    /*
     Positive retention window in caller timestamp units.
     */
    int64_t window;
    /*
     Current timestamp in the same unit.
     */
    int64_t now_ts;
} ZeRetentionRequest;

/*
 Physical-purge scheduling request.
 */
typedef struct ZePurgeRequest {
    /*
     Caller-provided structure size.
     */
    uint32_t abi_size;
    /*
     Must be zero in ABI v1.
     */
    uint32_t abi_reserved;
    /*
     Caller-owned document-id array.
     */
    const struct ZeDocId *doc_ids;
    /*
     Number of identifiers.
     */
    size_t doc_id_count;
} ZePurgeRequest;

/*
 Opaque physical-purge token report.
 */
typedef struct ZePurgeTokenReport {
    /*
     Caller-provided structure size.
     */
    uint32_t abi_size;
    /*
     Must be zero in ABI v1.
     */
    uint32_t abi_reserved;
    /*
     Store-scoped opaque token id.
     */
    uint64_t token_id;
    /*
     Generation observed when scheduled.
     */
    uint64_t generation;
    /*
     Number of requested ids absent from the store.
     */
    uint64_t unknown_id_count;
    /*
     One when no physical work is required.
     */
    uint32_t is_no_op;
    /*
     Must be zero.
     */
    uint32_t reserved;
} ZePurgeTokenReport;

/*
 Waits for one previously scheduled physical purge.
 */
typedef struct ZeAwaitPurgeRequest {
    /*
     Caller-provided structure size.
     */
    uint32_t abi_size;
    /*
     Must be zero in ABI v1.
     */
    uint32_t abi_reserved;
    /*
     Store-scoped opaque token id returned by `ze_purge`.
     */
    uint64_t token_id;
} ZeAwaitPurgeRequest;

/*
 Completed physical-purge report.
 */
typedef struct ZePurgeReport {
    /*
     Caller-provided structure size.
     */
    uint32_t abi_size;
    /*
     Must be zero in ABI v1.
     */
    uint32_t abi_reserved;
    /*
     Generation at which the physical guarantee holds.
     */
    uint64_t generation;
    /*
     Number of sealed segments rewritten.
     */
    uint64_t segments_rewritten;
    /*
     Number of requested ids absent from the store.
     */
    uint64_t unknown_id_count;
    /*
     One when the WAL was atomically replaced.
     */
    uint32_t wal_rewritten;
    /*
     One when no artifact needed mutation.
     */
    uint32_t is_no_op;
} ZePurgeReport;

/*
 Host-bounded maintenance request.
 */
typedef struct ZeMaintainRequest {
    /*
     Caller-provided structure size.
     */
    uint32_t abi_size;
    /*
     Must be zero in ABI v1.
     */
    uint32_t abi_reserved;
    /*
     Maximum monotonic wall time in nanoseconds.
     */
    uint64_t wall_time_ns;
    /*
     Maximum graph work bytes.
     */
    uint64_t bytes;
} ZeMaintainRequest;

/*
 Host-bounded maintenance report.
 */
typedef struct ZeMaintainReport {
    /*
     Caller-provided structure size.
     */
    uint32_t abi_size;
    /*
     Must be zero in ABI v1.
     */
    uint32_t abi_reserved;
    /*
     Graph generations published.
     */
    uint64_t graphs_built;
    /*
     Graph work bytes consumed.
     */
    uint64_t bytes_consumed;
    /*
     Checkpointed graph builds resumed.
     */
    uint64_t checkpoints_resumed;
    /*
     `0` complete or `1` budget exhausted.
     */
    int32_t status;
    /*
     Must be zero.
     */
    uint32_t reserved;
} ZeMaintainReport;

#ifdef __cplusplus
extern "C" {
#endif // __cplusplus

/*
 Returns the frozen ABI version, `ZE_ABI_VERSION`.
 */
uint32_t ze_abi_version(void);

/*
 Opens a store and writes a new generation-tagged handle.

 `path` is caller-owned UTF-8 bytes and need only outlive this call.
 Interior NUL bytes are rejected. `out_handle` is caller-owned.
 */
ze_error_code ze_open(const struct ZeOpenRequest *request,
                      ze_handle *out_handle);

/*
 Opens a store while declaring its embedding and tokenizer interpretation.
 `request` and `epoch` are caller-owned and need only outlive this call;
 every string and digest inside `epoch` is copied. A store that already
 carries a different identity returns `ZE_ERR_EPOCH_MISMATCH`. The declared
 identity is attached to every `ze_ingest` batch made through the handle,
 which a stamped store requires.
 */
ze_error_code ze_open_with_epoch(const struct ZeOpenRequest *request,
                                 const struct ZeEpochRequest *epoch,
                                 ze_handle *out_handle);

/*
 Computes the compact identity of a caller-declared epoch without touching
 any store. `epoch` is caller-owned; `out_identity` is caller-owned and
 must have `abi_size` initialized.
 */
ze_error_code ze_epoch_identity(const struct ZeEpochRequest *epoch,
                                struct ZeEpochIdentity *out_identity);

/*
 Reads the identity currently published by an open store. A store that
 carries no stamped epoch returns `ZE_ERR_EPOCH_UNSTAMPED`. `out_identity`
 is caller-owned and must have `abi_size` initialized.
 */
ze_error_code ze_epoch_current(ze_handle handle,
                               struct ZeEpochIdentity *out_identity);

/*
 Atomically publishes a registered epoch whose segments are retained.
 Requires a sealed active segment; a writer-slot conflict returns
 `ZE_ERR_BUSY`. Not cancellable in v1; the engine offers no token here.
 */
ze_error_code ze_epoch_switch_alias(ze_handle handle,
                                    const struct ZeEpochRequest *target,
                                    struct ZeEpochAliasReport *out_report);

/*
 Explicitly drops every immutable segment belonging to the embedding epoch
 named by `target.embedding`; the tokenizer profile is validated but not
 used. Not cancellable in v1; the engine offers no token here.
 */
ze_error_code ze_epoch_drop(ze_handle handle,
                            const struct ZeEpochRequest *target,
                            struct ZeEpochDropReport *out_report);

/*
 Releases the slot and store. A poisoned handle is still released and
 returns `ZE_ERR_POISONED`. All other calls reject poison before touching
 the store.
 */
ze_error_code ze_close(ze_handle handle);

/*
 Reads the explicit store lifecycle state. `out_report` is caller-owned
 and must have `abi_size` initialized.
 */
ze_error_code ze_state(ze_handle handle, struct ZeStateReport *out_report);

/*
 Reads exact resource counters for an open store. `out_report` is
 caller-owned and must have `abi_size` initialized.
 */
ze_error_code ze_stats(ze_handle handle, struct ZeStatsReport *out_report);

/*
 Atomically ingests caller-owned document records. Every const pointer
 is caller-owned and need only outlive the call. A record with nonzero
 `text_len` is analyzed into the lexical index with the ingest tokenizer.
 Not cancellable in v1; the engine offers no token here.
 */
ze_error_code ze_ingest(ze_handle handle,
                        const struct ZeIngestRequest *request,
                        struct ZeMutationReport *out_report);

/*
 Atomically tombstones caller-owned document identifiers. Every const
 pointer is caller-owned and need only outlive the call. Not cancellable
 in v1; the engine offers no token here.
 */
ze_error_code ze_delete(ze_handle handle,
                        const struct ZeDeleteRequest *request,
                        struct ZeMutationReport *out_report);

/*
 Searches active and immutable store state with optional cancellation.
 The query vector is caller-owned for the call. On success `hits` is
 callee-owned and must be released exactly once with
 `ze_search_result_free`. A zeroed result and a second free of the same
 result object are safe.
 */
ze_error_code ze_search(ze_handle handle,
                        const struct ZeSearchRequest *request,
                        struct ZeSearchResult *out_result);

/*
 Runs one structured query: a vector leg, a lexical leg, or exact hybrid
 fusion of both. Every request pointer is caller-owned for the call. On
 success `hits` is callee-owned and must be released exactly once with
 `ze_query_result_free`. A zeroed result and a second free are safe.
 */
ze_error_code ze_query(ze_handle handle,
                       const struct ZeQueryRequest *request,
                       struct ZeQueryResult *out_result);

/*
 Releases a callee-owned query hit array; a zeroed result is a successful
 no-op. `result` is caller-owned; only its `hits` allocation is released.
 */
ze_error_code ze_query_result_free(struct ZeQueryResult *result);

/*
 Seals the active segment. Cancellable through `request.cancel_token`.
 */
ze_error_code ze_seal(ze_handle handle,
                      const struct ZeSealRequest *request,
                      struct ZeGenerationReport *out_report);

/*
 Drops immutable segments wholly contained by a timestamp range. Not
 cancellable in v1; the engine offers no token here.
 */
ze_error_code ze_drop_partition(ze_handle handle,
                                const struct ZeDropPartitionRequest *request,
                                struct ZePartitionReport *out_report);

/*
 Applies a positive retention window at a caller-supplied timestamp. Not
 cancellable in v1; the engine offers no token here.
 */
ze_error_code ze_apply_retention(ze_handle handle,
                                 const struct ZeRetentionRequest *request,
                                 struct ZePartitionReport *out_report);

/*
 Schedules physical removal of document ids and returns an opaque token.
 Not cancellable in v1; the engine offers no token here.
 */
ze_error_code ze_purge(ze_handle handle,
                       const struct ZePurgeRequest *request,
                       struct ZePurgeTokenReport *out_report);

/*
 Waits until a scheduled purge has removed every reachable physical byte.
 Not cancellable in v1; the engine offers no token here.
 */
ze_error_code ze_await_physical_purge(ze_handle handle,
                                      const struct ZeAwaitPurgeRequest *request,
                                      struct ZePurgeReport *out_report);

/*
 Runs due tier transitions within caller-supplied work budgets. Not
 cancellable in v1; the engine offers no token here.
 */
ze_error_code ze_maintain(ze_handle handle,
                          const struct ZeMaintainRequest *request,
                          struct ZeMaintainReport *out_report);

/*
 Copies the per-handle or process-global last error into caller-owned
 memory. `buffer` and `written` are caller-owned. Capacity zero with a
 non-null `written` is a legal size probe. Pass handle zero for pre-handle
 or global errors. This is the sole store-handle accessor that remains
 usable after poisoning.
 */
ze_error_code ze_last_error_message(ze_handle handle,
                                    char *buffer,
                                    size_t capacity,
                                    size_t *written);

/*
 Returns a static NUL-terminated symbolic name for one numeric error code.
 The string must never be freed.
 */
const char *ze_error_code_name(int32_t code);

/*
 Creates an active generation-tagged cancellation token.
 */
ze_error_code ze_cancel_token_create(ze_cancel_token *out_token);

/*
 Requests cancellation; repeated requests are harmless.
 */
ze_error_code ze_cancel_token_cancel(ze_cancel_token token);

/*
 Releases a cancellation token; a stale second free returns `Closed`.
 */
ze_error_code ze_cancel_token_free(ze_cancel_token token);

/*
 Releases a callee-owned hit array; a zeroed result is a successful no-op.
 `result` is caller-owned; only its `hits` allocation is released.
 */
ze_error_code ze_search_result_free(struct ZeSearchResult *result);

#ifdef __cplusplus
}  // extern "C"
#endif  // __cplusplus

#endif  /* ZEPPELIN_EMBED_H */
