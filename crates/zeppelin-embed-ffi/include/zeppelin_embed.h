#ifndef ZEPPELIN_EMBED_H
#define ZEPPELIN_EMBED_H

#include <stddef.h>
#include <stdint.h>

#ifdef __cplusplus
extern "C" {
#endif

#define ZE_ABI_VERSION 1u
#define ZE_ABI_MAX_STRUCT_SIZE 65536u
#define ZE_MAX_K (1u << 20)

typedef uint64_t ze_handle;
typedef uint64_t ze_cancel_token;

/* Append-only. Existing numeric values must never be changed or reused. */
typedef enum ze_error_code {
    ZE_OK = 0,
    ZE_ERR_INVALID_ARGUMENT = 1,
    ZE_ERR_INVALID_HANDLE = 2,
    ZE_ERR_CLOSED = 3,
    ZE_ERR_CLOSING = 4,
    ZE_ERR_POISONED = 5,
    ZE_ERR_PANIC = 6,
    ZE_ERR_BUSY = 7,
    ZE_ERR_STORE_BUSY = 8,
    ZE_ERR_IO = 9,
    ZE_ERR_CORRUPT = 10,
    ZE_ERR_UNSUPPORTED = 11,
    ZE_ERR_CANCELLED = 12,
    ZE_ERR_TIMEOUT = 13,
    ZE_ERR_OUT_OF_MEMORY = 14,
    ZE_ERR_BUDGET_EXCEEDED = 15,
    ZE_ERR_EMPTY_BATCH = 16,
    ZE_ERR_STALE_REVISION = 17,
    ZE_ERR_DIMENSION_MISMATCH = 18,
    ZE_ERR_NOT_FOUND = 19,
    ZE_ERR_SYNCHRONIZATION = 20,
    ZE_ERR_ACCESS_MODE = 21,
    ZE_ERR_INTERNAL = 22
} ze_error_code;

typedef struct ZeOpenRequest {
    uint32_t abi_size;
    uint32_t abi_reserved;
    const uint8_t *path;
    size_t path_len;
    int32_t access_mode;
    int32_t durability_mode;
    int32_t commit_tier;
    uint64_t reader_drain_timeout_ms;
    uint64_t max_resident_bytes;
    uint64_t max_temp_bytes;
} ZeOpenRequest;

typedef struct ZeStateReport {
    uint32_t abi_size;
    uint32_t abi_reserved;
    int32_t state;
    uint32_t reserved;
} ZeStateReport;

typedef struct ZeStatsReport {
    uint32_t abi_size;
    uint32_t abi_reserved;
    uint64_t resident_owned_bytes;
    uint64_t mapped_bytes;
    uint64_t mapped_resident_bytes;
    uint64_t segment_bytes;
    uint64_t active_segment_bytes;
    uint64_t active_row_count;
    uint64_t tombstone_count;
    uint64_t tombstone_bytes;
    uint64_t wal_bytes;
    uint64_t cache_bytes;
    uint64_t temporary_bytes;
    uint64_t query_pool_bytes;
    uint64_t open_files;
    uint64_t active_queries;
    uint64_t active_snapshot_leases;
    uint64_t phys_footprint;
    uint32_t has_phys_footprint;
    uint32_t reserved;
} ZeStatsReport;

typedef struct ZeDocId {
    uint64_t high;
    uint64_t low;
} ZeDocId;

typedef struct ZeIngestDocument {
    uint32_t abi_size;
    uint32_t abi_reserved;
    ZeDocId doc_id;
    uint64_t revision;
    int64_t timestamp;
    const float *vector;
    size_t vector_len;
    const uint8_t *metadata;
    size_t metadata_len;
} ZeIngestDocument;

typedef struct ZeIngestRequest {
    uint32_t abi_size;
    uint32_t abi_reserved;
    const ZeIngestDocument *documents;
    size_t document_count;
    size_t dimension;
} ZeIngestRequest;

typedef struct ZeDeleteRequest {
    uint32_t abi_size;
    uint32_t abi_reserved;
    const ZeDocId *doc_ids;
    size_t doc_id_count;
} ZeDeleteRequest;

typedef struct ZeMutationReport {
    uint32_t abi_size;
    uint32_t abi_reserved;
    uint64_t sequence;
    uint64_t generation;
} ZeMutationReport;

typedef struct ZeSearchRequest {
    uint32_t abi_size;
    uint32_t abi_reserved;
    const float *vector;
    size_t vector_len;
    size_t dimension;
    size_t k;
    size_t thread_budget;
    int32_t search_tier;
    int32_t graph_profile;
    size_t graph_ef;
    uint64_t graph_seed;
    ze_cancel_token cancel_token;
    uint64_t deadline_ns;
} ZeSearchRequest;

typedef struct ZeSearchHit {
    uint32_t source_kind;
    uint32_t reserved;
    uint8_t segment_id[16];
    uint32_t local_row;
    uint32_t has_document;
    ZeDocId doc_id;
    uint64_t revision;
    float score;
    uint32_t reserved_tail;
} ZeSearchHit;

typedef struct ZeSearchResult {
    uint32_t abi_size;
    uint32_t abi_reserved;
    ZeSearchHit *hits;
    size_t hit_count;
    uint64_t generation;
    uint64_t dims_touched;
    uint64_t bytes_read;
    uint64_t threads_used;
    uint64_t graph_segments_traversed;
    uint64_t graph_validations;
    uint64_t graph_entry_seed_discoveries;
    uint64_t graph_visited_epoch_clears;
    uint64_t graph_candidates_scored;
    uint64_t graph_candidates_rescored;
    uint64_t graph_segments_pruned_by_bound;
} ZeSearchResult;

typedef struct ZeSealRequest {
    uint32_t abi_size;
    uint32_t abi_reserved;
    ze_cancel_token cancel_token;
} ZeSealRequest;

typedef struct ZeGenerationReport {
    uint32_t abi_size;
    uint32_t abi_reserved;
    uint64_t generation;
} ZeGenerationReport;

typedef struct ZeDropPartitionRequest {
    uint32_t abi_size;
    uint32_t abi_reserved;
    int64_t start_ts;
    int64_t end_ts;
} ZeDropPartitionRequest;

typedef struct ZeRetentionRequest {
    uint32_t abi_size;
    uint32_t abi_reserved;
    int64_t window;
    int64_t now_ts;
} ZeRetentionRequest;

typedef struct ZePartitionReport {
    uint32_t abi_size;
    uint32_t abi_reserved;
    uint64_t generation;
    uint64_t segments_dropped;
    uint64_t bytes_reclaimed;
    uint64_t straddlers_skipped;
    uint32_t is_no_op;
    uint32_t reserved;
} ZePartitionReport;

typedef struct ZePurgeRequest {
    uint32_t abi_size;
    uint32_t abi_reserved;
    const ZeDocId *doc_ids;
    size_t doc_id_count;
} ZePurgeRequest;

typedef struct ZePurgeTokenReport {
    uint32_t abi_size;
    uint32_t abi_reserved;
    uint64_t token_id;
    uint64_t generation;
    uint64_t unknown_id_count;
    uint32_t is_no_op;
    uint32_t reserved;
} ZePurgeTokenReport;

typedef struct ZeAwaitPurgeRequest {
    uint32_t abi_size;
    uint32_t abi_reserved;
    uint64_t token_id;
} ZeAwaitPurgeRequest;

typedef struct ZePurgeReport {
    uint32_t abi_size;
    uint32_t abi_reserved;
    uint64_t generation;
    uint64_t segments_rewritten;
    uint64_t unknown_id_count;
    uint32_t wal_rewritten;
    uint32_t is_no_op;
} ZePurgeReport;

typedef struct ZeMaintainRequest {
    uint32_t abi_size;
    uint32_t abi_reserved;
    uint64_t wall_time_ns;
    uint64_t bytes;
} ZeMaintainRequest;

typedef struct ZeMaintainReport {
    uint32_t abi_size;
    uint32_t abi_reserved;
    uint64_t graphs_built;
    uint64_t bytes_consumed;
    uint64_t checkpoints_resumed;
    int32_t status;
    uint32_t reserved;
} ZeMaintainReport;

uint32_t ze_abi_version(void);

/* path is caller-owned UTF-8 bytes and need only outlive this call. Interior
 * NUL bytes are rejected. out_handle is caller-owned. */
ze_error_code ze_open(const ZeOpenRequest *request, ze_handle *out_handle);

/* Releases the slot and Store. A poisoned handle is still released and returns
 * ZE_ERR_POISONED. All other calls reject poison before touching the Store. */
ze_error_code ze_close(ze_handle handle);

/* out_report is caller-owned and must have abi_size initialized. */
ze_error_code ze_state(ze_handle handle, ZeStateReport *out_report);
ze_error_code ze_stats(ze_handle handle, ZeStatsReport *out_report);

/* All const pointers are caller-owned and need only outlive the call.
 * Not cancellable in v1 -- the engine offers no token here. */
ze_error_code ze_ingest(ze_handle handle, const ZeIngestRequest *request,
                        ZeMutationReport *out_report);
ze_error_code ze_delete(ze_handle handle, const ZeDeleteRequest *request,
                        ZeMutationReport *out_report);

/* The query vector is caller-owned for the call. On success hits is
 * callee-owned and must be released exactly once with ze_search_result_free.
 * A zeroed result and a second free of the same result object are safe. */
ze_error_code ze_search(ze_handle handle, const ZeSearchRequest *request,
                        ZeSearchResult *out_result);

/* Seal is cancellable through request.cancel_token. */
ze_error_code ze_seal(ze_handle handle, const ZeSealRequest *request,
                      ZeGenerationReport *out_report);

/* Not cancellable in v1 -- the engine offers no token here. */
ze_error_code ze_drop_partition(ze_handle handle,
                                const ZeDropPartitionRequest *request,
                                ZePartitionReport *out_report);
/* Not cancellable in v1 -- the engine offers no token here. */
ze_error_code ze_apply_retention(ze_handle handle,
                                 const ZeRetentionRequest *request,
                                 ZePartitionReport *out_report);
/* Not cancellable in v1 -- the engine offers no token here. */
ze_error_code ze_purge(ze_handle handle, const ZePurgeRequest *request,
                       ZePurgeTokenReport *out_report);
/* Not cancellable in v1 -- the engine offers no token here. */
ze_error_code ze_await_physical_purge(ze_handle handle,
                                      const ZeAwaitPurgeRequest *request,
                                      ZePurgeReport *out_report);
/* Not cancellable in v1 -- the engine offers no token here. */
ze_error_code ze_maintain(ze_handle handle, const ZeMaintainRequest *request,
                          ZeMaintainReport *out_report);

/* buffer and written are caller-owned. capacity zero with non-null written is
 * a legal size probe. Pass handle zero for pre-handle/global errors. This is
 * the sole store-handle accessor that remains usable after poisoning. */
ze_error_code ze_last_error_message(ze_handle handle, char *buffer,
                                    size_t capacity, size_t *written);

/* Returns a static NUL-terminated string that must never be freed. */
const char *ze_error_code_name(ze_error_code code);

ze_error_code ze_cancel_token_create(ze_cancel_token *out_token);
ze_error_code ze_cancel_token_cancel(ze_cancel_token token);
ze_error_code ze_cancel_token_free(ze_cancel_token token);

/* result is caller-owned; only its hits allocation is released. */
ze_error_code ze_search_result_free(ZeSearchResult *result);

/* Reserved, intentionally not exported:
 *   ze_health, ze_diagnostics -- unblocked by task 18 (diag.rs)
 *   ze_epoch_*                -- unblocked by task 21 (epoch.rs)
 *   ze_search_filtered        -- task 22 phase 2
 *   ze_scan_top_k             -- task 22 phase 2
 */

#ifdef __cplusplus
}
#endif

#endif
