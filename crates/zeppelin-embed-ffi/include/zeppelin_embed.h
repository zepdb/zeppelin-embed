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
 Treat the last analyzed lexical query term as a type-ahead prefix.
 */
#define ZE_QUERY_LAST_AS_PREFIX 1

/*
 Largest `group_limit` accepted by `ze_count_grouped`.
 */
#define ZE_MAX_COUNT_GROUPS (1 << 16)

/*
 No condition: the document is written whatever its live revision.
 */
#define ZE_REVISION_CONDITION_NONE 0

/*
 A live document must exist at exactly `ZeRevisionCondition::revision`.
 */
#define ZE_REVISION_CONDITION_EXACTLY 1

/*
 No live document may exist: the id was never written or was deleted.
 */
#define ZE_REVISION_CONDITION_ABSENT 2

/*
 `manifest.ze` is absent, but the WAL or segment files prove a committed
 snapshot existed and data it covered is now unreachable.
 */
#define ZE_VERIFY_MANIFEST_MISSING 1

/*
 The manifest frame, checksum, or payload failed to decode.
 */
#define ZE_VERIFY_MANIFEST_CORRUPT 2

/*
 The manifest covers WAL sequences that the WAL does not hold.
 */
#define ZE_VERIFY_MANIFEST_AHEAD_OF_WAL 3

/*
 A segment the manifest references does not exist.
 */
#define ZE_VERIFY_SEGMENT_MISSING 4

/*
 A segment header, length, identity, or file trailer failed validation.
 */
#define ZE_VERIFY_SEGMENT_CORRUPT 5

/*
 A segment header disagrees with the manifest's record of it.
 */
#define ZE_VERIFY_SEGMENT_MISMATCH 6

/*
 A segment region's checksum does not match its bytes.
 */
#define ZE_VERIFY_SEGMENT_REGION_CORRUPT 7

/*
 A checksum-valid region failed its decoder or cross-structure checks.
 */
#define ZE_VERIFY_SEGMENT_INDEX_INVALID 8

/*
 The WAL is absent although the manifest covers WAL sequences.
 */
#define ZE_VERIFY_WAL_MISSING 9

/*
 The WAL file header is truncated or invalid.
 */
#define ZE_VERIFY_WAL_HEADER_CORRUPT 10

/*
 A WAL record failed framing, checksum, or sequence validation.
 */
#define ZE_VERIFY_WAL_RECORD_CORRUPT 11

/*
 A checksum-valid WAL record cannot be replayed into the store.
 */
#define ZE_VERIFY_WAL_RECORD_INVALID 12

/*
 A store file exists but could not be read.
 */
#define ZE_VERIFY_UNREADABLE 13

/*
 The pending purge intent `purge.ze` failed its frame or decoder.
 */
#define ZE_VERIFY_PURGE_INTENT_CORRUPT 14

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
    /*
     A text model bundle was absent, malformed, or corrupt.
     */
    ZE_ERR_BUNDLE = 29,
    /*
     The configured text model runtime failed.
     */
    ZE_ERR_MODEL = 30,
    /*
     A bounded text pipeline stage failed.
     */
    ZE_ERR_PIPELINE = 31,
    /*
     A scan continuation no longer names the current store generation.
     */
    ZE_ERR_SCAN_STALE = 32,
    /*
     A declared namespace schema differs from the persisted schema.
     */
    ZE_ERR_SCHEMA_MISMATCH = 33,
    /*
     The requested operation requires a vector space.
     */
    ZE_ERR_NO_VECTOR_SPACE = 34,
    /*
     The directory contains a different store kind.
     */
    ZE_ERR_STORE_KIND = 35,
    /*
     A required format version is unsupported.
     */
    ZE_ERR_FORMAT_VERSION = 36,
    /*
     Query text is syntactically invalid.
     */
    ZE_ERR_QUERY_SYNTAX = 37,
    /*
     A query feature is outside the supported profile.
     */
    ZE_ERR_QUERY_UNSUPPORTED = 38,
    /*
     A required parameter is missing or invalid.
     */
    ZE_ERR_PARAMETER = 39,
    /*
     A value has an incompatible query type.
     */
    ZE_ERR_TYPE = 40,
    /*
     A query binding is outside its legal scope.
     */
    ZE_ERR_SCOPE = 41,
    /*
     An application key conflicts with an existing record.
     */
    ZE_ERR_KEY_CONFLICT = 42,
    /*
     An expected entity incarnation does not match.
     */
    ZE_ERR_INCARNATION_CONFLICT = 43,
    /*
     An expected deletion revision does not match.
     */
    ZE_ERR_DELETION_REVISION_CONFLICT = 44,
    /*
     A relationship endpoint is invalid or unavailable.
     */
    ZE_ERR_ENDPOINT = 45,
    /*
     A query attempted to use a deleted entity.
     */
    ZE_ERR_DELETED_ENTITY = 46,
    /*
     An arithmetic operand is outside the operation domain.
     */
    ZE_ERR_ARITHMETIC_DOMAIN = 47,
    /*
     Checked arithmetic overflowed.
     */
    ZE_ERR_ARITHMETIC_OVERFLOW = 48,
    /*
     An arithmetic divisor was zero.
     */
    ZE_ERR_DIVISION_BY_ZERO = 49,
    /*
     The coordinator cannot yet establish the durable write outcome.
     */
    ZE_ERR_INDETERMINATE_COMMIT = 50,
    /*
     An entity revision cannot advance.
     */
    ZE_ERR_REVISION_OVERFLOW = 51,
    /*
     The graph generation cannot advance.
     */
    ZE_ERR_GENERATION_OVERFLOW = 52,
    /*
     A batch names the same mutation target more than once.
     */
    ZE_ERR_DUPLICATE_TARGET = 53,
    /*
     The available identity space is exhausted.
     */
    ZE_ERR_IDENTITY_OVERFLOW = 54,
    /*
     A document's expected-revision condition did not hold; nothing was
     written.
     */
    ZE_ERR_REVISION_CONFLICT = 55,
    /*
     A persisted format is newer than this build can read.
     */
    ZE_ERR_FORMAT_TOO_NEW = 56,
    /*
     A namespace cascade declaration closes a cycle.
     */
    ZE_ERR_CASCADE_CYCLE = 57,
    /*
     A graph-only directory cannot be opened as a legacy document store.
     */
    ZE_ERR_LEGACY_GRAPH_DIRECTORY = 58,
    /*
     This build cannot open a store containing graph state.
     */
    ZE_ERR_GRAPH_UNSUPPORTED_BUILD = 59,
    /*
     Graph catalogs cannot yet carry an epoch alias switch or epoch drop.
     */
    ZE_ERR_GRAPH_EPOCH_TRANSITION = 60,
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
     `0` textDefault, `1` code, or `2` voice tokenizer profile.
     */
    int32_t tokenizer_profile;
    /*
     Must be zero.
     */
    uint32_t reserved;
} ZeEpochRequest;

/*
 One caller-owned namespace attribute declaration.
 */
typedef struct ZeAttributeDefinition {
    /*
     Schema-local identifier; zero is reserved for the `ts` column.
     */
    uint32_t attribute_id;
    /*
     Caller-owned UTF-8 attribute name.
     */
    const uint8_t *name;
    /*
     Number of attribute-name bytes.
     */
    size_t name_len;
    /*
     `1` U64, `2` I64, `3` F64, `4` Bool, `5` dictionary string, `6` raw string, or `7` Id128.
     */
    int32_t attribute_type;
    /*
     One when the attribute is nullable.
     */
    uint32_t nullable;
} ZeAttributeDefinition;

/*
 Schema and vector-space identity declared for one namespace.
 */
typedef struct ZeNamespaceSpec {
    /*
     Caller-provided `sizeof(ZeNamespaceSpec)`.
     */
    uint32_t abi_size;
    /*
     Must be zero in ABI v1.
     */
    uint32_t abi_reserved;
    /*
     Caller-owned attribute definitions.
     */
    const struct ZeAttributeDefinition *attributes;
    /*
     Number of attribute definitions.
     */
    size_t attribute_count;
    /*
     One for a vector namespace, zero for a record-only namespace.
     */
    uint32_t has_vector_space;
    /*
     Vector dimensions, or zero or one for a record-only namespace.
     */
    uint32_t dimensions;
    /*
     `0` for none or `1` for unit-L2 normalization.
     */
    int32_t normalization;
    /*
     Optional caller-owned epoch; null selects the canonical namespace epoch.
     */
    const struct ZeEpochRequest *epoch;
} ZeNamespaceSpec;

/*
 Opens or idempotently creates one namespace under a database root.
 */
typedef struct ZeNamespaceOpenRequest {
    /*
     Caller-provided `sizeof(ZeNamespaceOpenRequest)`.
     */
    uint32_t abi_size;
    /*
     Must be zero in ABI v1.
     */
    uint32_t abi_reserved;
    /*
     Caller-owned UTF-8 database-root path.
     */
    const uint8_t *root;
    /*
     Number of root-path bytes.
     */
    size_t root_len;
    /*
     Caller-owned namespace-name bytes.
     */
    const uint8_t *name;
    /*
     Number of namespace-name bytes.
     */
    size_t name_len;
    /*
     Existing store-open settings; its path fields are ignored.
     */
    struct ZeOpenRequest open;
    /*
     Required namespace schema and vector-space declaration.
     */
    const struct ZeNamespaceSpec *spec;
} ZeNamespaceOpenRequest;

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
 One typed schema attribute value supplied to an upsert.
 */
typedef struct ZeAttributeValue {
    /*
     Schema-local identifier; zero is reserved for the `ts` column.
     */
    uint32_t attribute_id;
    /*
     `0` null, `1` U64, `2` I64, `3` F64, `4` Bool, `5` string, or `6` Id128.
     */
    int32_t value_type;
    /*
     Unsigned-integer payload when `value_type` is one; low 64 bits for Id128.
     */
    uint64_t u64_value;
    /*
     Signed-integer payload when `value_type` is two; high 64 bits for Id128
     interpreted as an unsigned bit pattern (not a signed numeric value).
     */
    int64_t i64_value;
    /*
     Floating-point payload when `value_type` is three.
     */
    double f64_value;
    /*
     Boolean payload when `value_type` is four.
     */
    uint32_t bool_value;
    /*
     Caller-owned UTF-8 bytes when `value_type` is five.
     */
    const uint8_t *string_value;
    /*
     Number of string bytes.
     */
    size_t string_len;
} ZeAttributeValue;

/*
 One ingest document plus its typed schema attributes.
 */
typedef struct ZeUpsertDocument {
    /*
     Caller-provided `sizeof(ZeUpsertDocument)`.
     */
    uint32_t abi_size;
    /*
     Must be zero in ABI v1.
     */
    uint32_t abi_reserved;
    /*
     Existing v1 ingest document, embedded by value.
     */
    struct ZeIngestDocument document;
    /*
     Caller-owned attribute-value array.
     */
    const struct ZeAttributeValue *attributes;
    /*
     Number of attribute values.
     */
    size_t attribute_count;
} ZeUpsertDocument;

/*
 Atomic document upsert request with typed schema attributes.
 */
typedef struct ZeUpsertRequest {
    /*
     Caller-provided `sizeof(ZeUpsertRequest)`.
     */
    uint32_t abi_size;
    /*
     Must be zero in ABI v1.
     */
    uint32_t abi_reserved;
    /*
     Caller-owned `ZeUpsertDocument` array.
     */
    const struct ZeUpsertDocument *documents;
    /*
     Number of document records.
     */
    size_t document_count;
    /*
     Vector dimension for every record.
     */
    size_t dimension;
} ZeUpsertRequest;

/*
 One document's live-revision precondition.

 The live revision is the revision `ze_get` returns for the id; a deleted
 id has none. Conditions are checked by the store's single writer against
 the latest committed state, before the batch is applied and atomically
 with it.
 */
typedef struct ZeRevisionCondition {
    /*
     One of the `ZE_REVISION_CONDITION_*` constants.
     */
    uint32_t kind;
    /*
     Must be zero.
     */
    uint32_t reserved;
    /*
     Expected live revision when `kind` is `ZE_REVISION_CONDITION_EXACTLY`;
     must be zero otherwise.
     */
    uint64_t revision;
} ZeRevisionCondition;

/*
 Atomic upsert that commits only when every document's condition holds.
 */
typedef struct ZeConditionalUpsertRequest {
    /*
     Caller-provided `sizeof(ZeConditionalUpsertRequest)`.
     */
    uint32_t abi_size;
    /*
     Must be zero in ABI v1.
     */
    uint32_t abi_reserved;
    /*
     Existing v1 upsert request, embedded by value.
     */
    struct ZeUpsertRequest batch;
    /*
     Caller-owned conditions; entry `i` applies to `batch.documents[i]`.
     */
    const struct ZeRevisionCondition *conditions;
    /*
     Must equal `batch.document_count`.
     */
    size_t condition_count;
} ZeConditionalUpsertRequest;

/*
 One flat filter-AST node; child nodes are referenced by an index range.
 */
typedef struct ZeFilterNode {
    /*
     `1` eq, `2` not-eq, `3` in, `4` not-in, `5` range, `6` exists,
     `7` is-null, `8` and, `9` or, or `10` not.
     */
    int32_t op;
    /*
     Schema-local column identifier for leaf operators.
     */
    uint32_t attribute_id;
    /*
     Caller-owned values for equality and membership operators.
     */
    const struct ZeAttributeValue *values;
    /*
     Number of entries in `values`.
     */
    size_t value_count;
    /*
     One when `lower` is present.
     */
    uint32_t has_lower;
    /*
     Lower range endpoint.
     */
    struct ZeAttributeValue lower;
    /*
     One when the lower endpoint is inclusive.
     */
    uint32_t lower_inclusive;
    /*
     One when `upper` is present.
     */
    uint32_t has_upper;
    /*
     Upper range endpoint.
     */
    struct ZeAttributeValue upper;
    /*
     One when the upper endpoint is inclusive.
     */
    uint32_t upper_inclusive;
    /*
     First child node index for logical operators.
     */
    uint32_t children_start;
    /*
     Number of consecutive child node indices.
     */
    uint32_t children_count;
} ZeFilterNode;

/*
 Caller-owned flat structured filter.
 */
typedef struct ZeFilter {
    /*
     Caller-provided `sizeof(ZeFilter)`.
     */
    uint32_t abi_size;
    /*
     Must be zero in ABI v1.
     */
    uint32_t abi_reserved;
    /*
     Caller-owned flat node array.
     */
    const struct ZeFilterNode *nodes;
    /*
     Number of nodes in `nodes`.
     */
    size_t node_count;
    /*
     Root node index.
     */
    uint32_t root;
} ZeFilter;

/*
 One participant of ze_namespace_batch; all pointers are caller-owned.
 */
typedef struct ZeNamespaceMutation {
    /*
     sizeof(ZeNamespaceMutation).
     */
    uint32_t abi_size;
    /*
     Zero.
     */
    uint32_t abi_reserved;
    /*
     Existing namespace name.
     */
    const uint8_t *name;
    /*
     Name byte length.
     */
    size_t name_len;
    /*
     Existing namespace declaration; schema evolution is not allowed here.
     */
    const struct ZeNamespaceSpec *spec;
    /*
     0 textDefault, 1 code, 2 voice.
     */
    int32_t tokenizer_profile;
    /*
     Zero.
     */
    uint32_t reserved;
    /*
     Upserts; document_count zero skips this phase.
     */
    struct ZeConditionalUpsertRequest upserts;
    /*
     Explicit document IDs to delete after upserts.
     */
    const struct ZeDocId *deletes;
    /*
     Number of delete IDs.
     */
    size_t delete_count;
    /*
     Optional predicate deletion after explicit changes; null skips it.
     */
    const struct ZeFilter *filter;
} ZeNamespaceMutation;

/*
 One fully durable transaction over 2..128 existing namespaces of one root.
 */
typedef struct ZeNamespaceBatchRequest {
    /*
     sizeof(ZeNamespaceBatchRequest).
     */
    uint32_t abi_size;
    /*
     Zero.
     */
    uint32_t abi_reserved;
    /*
     UTF-8 root path.
     */
    const uint8_t *root;
    /*
     Root byte length.
     */
    size_t root_len;
    /*
     Caller-owned participants; unique names, in result order.
     */
    const struct ZeNamespaceMutation *participants;
    /*
     Participant count.
     */
    size_t participant_count;
    /*
     Caller-owned output of participant_count u64 generations, written on success.
     */
    uint64_t *generations;
} ZeNamespaceBatchRequest;

/*
 One cascade declaration referencing participants of a namespace request.
 */
typedef struct ZeCascadeDeclaration {
    /*
     sizeof(ZeCascadeDeclaration).
     */
    uint32_t abi_size;
    /*
     Zero.
     */
    uint32_t abi_reserved;
    /*
     Index of the existing parent namespace participant.
     */
    uint32_t parent_index;
    /*
     Index of the existing child namespace participant.
     */
    uint32_t child_index;
    /*
     Child schema's id128 attribute ID.
     */
    uint32_t attribute_id;
} ZeCascadeDeclaration;

/*
 Live writable handles paired with the batch participants in input order.
 Handles must belong to this process. Outputs are written only on success.
 */
typedef struct ZeNamespaceBatchLiveRequest {
    /*
     sizeof(ZeNamespaceBatchLiveRequest).
     */
    uint32_t abi_size;
    /*
     Zero.
     */
    uint32_t abi_reserved;
    /*
     Root, mutations and caller-owned generation outputs.
     */
    struct ZeNamespaceBatchRequest batch;
    /*
     Caller-owned array of batch.participant_count live store handles.
     */
    const ze_handle *handles;
} ZeNamespaceBatchLiveRequest;

/*
 Requests namespace discovery immediately below one database root.
 */
typedef struct ZeNamespaceListRequest {
    /*
     Caller-provided `sizeof(ZeNamespaceListRequest)`.
     */
    uint32_t abi_size;
    /*
     Must be zero in ABI v1.
     */
    uint32_t abi_reserved;
    /*
     Caller-owned UTF-8 database-root path.
     */
    const uint8_t *root;
    /*
     Number of root-path bytes.
     */
    size_t root_len;
} ZeNamespaceListRequest;

/*
 One callee-owned namespace name.
 */
typedef struct ZeNamespaceEntry {
    /*
     Name bytes owned by the containing result arena.
     */
    const uint8_t *name;
    /*
     Number of name bytes.
     */
    size_t name_len;
} ZeNamespaceEntry;

/*
 Callee-owned namespace list; release with `ze_namespace_list_result_free`.
 */
typedef struct ZeNamespaceListResult {
    /*
     Caller-provided structure size.
     */
    uint32_t abi_size;
    /*
     Caller sets zero; callee returns an opaque allocation generation.
     */
    uint32_t abi_reserved;
    /*
     Callee-owned entry array, or null when `entry_count` is zero.
     */
    struct ZeNamespaceEntry *entries;
    /*
     Number of initialized entries.
     */
    size_t entry_count;
} ZeNamespaceListResult;

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
 Text store plus immutable model-bundle open request.
 */
typedef struct ZeTextOpenRequest {
    /*
     Caller-provided structure size.
     */
    uint32_t abi_size;
    /*
     Must be zero.
     */
    uint32_t abi_reserved;
    /*
     Existing core store-open fields.
     */
    struct ZeOpenRequest store;
    /*
     Caller-owned UTF-8 `.zem` path.
     */
    const uint8_t *bundle_path;
    /*
     Number of bundle-path bytes.
     */
    size_t bundle_path_len;
} ZeTextOpenRequest;

/*
 One caller-owned text document.
 */
typedef struct ZeTextDocument {
    /*
     Caller-provided structure size.
     */
    uint32_t abi_size;
    /*
     Must be zero.
     */
    uint32_t abi_reserved;
    /*
     Stable caller document id; only the high 96-bit text namespace fits.
     */
    struct ZeDocId doc_id;
    /*
     Monotonic document revision.
     */
    uint64_t revision;
    /*
     Caller-owned UTF-8 text.
     */
    const uint8_t *text;
    /*
     Number of text bytes.
     */
    size_t text_len;
} ZeTextDocument;

/*
 Bounded text-ingest request.
 */
typedef struct ZeTextIngestRequest {
    /*
     Caller-provided structure size.
     */
    uint32_t abi_size;
    /*
     Must be zero.
     */
    uint32_t abi_reserved;
    /*
     Caller-owned document array.
     */
    const struct ZeTextDocument *documents;
    /*
     Number of document records.
     */
    size_t document_count;
    /*
     Model rows evaluated in one MLX call.
     */
    size_t embed_batch_size;
    /*
     Documents between immutable seal boundaries.
     */
    size_t seal_every;
    /*
     Bounded tokenized-batch channel capacity.
     */
    size_t channel_capacity;
} ZeTextIngestRequest;

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

/*
 Single-pass text query request.
 */
typedef struct ZeTextQueryRequest {
    /*
     Caller-provided structure size.
     */
    uint32_t abi_size;
    /*
     Must be zero.
     */
    uint32_t abi_reserved;
    /*
     Caller-owned UTF-8 query text without a model prefix.
     */
    const uint8_t *text;
    /*
     Number of query bytes.
     */
    size_t text_len;
    /*
     Requested hit count.
     */
    size_t k;
    /*
     Dense, lexical, or hybrid.
     */
    int32_t legs;
    /*
     Must be zero.
     */
    uint32_t reserved;
} ZeTextQueryRequest;

/*
 One callee-owned text query hit.
 */
typedef struct ZeTextQueryHit {
    /*
     Original caller document id.
     */
    struct ZeDocId doc_id;
    /*
     Document revision.
     */
    uint64_t revision;
    /*
     Chunk index.
     */
    uint32_t chunk;
    /*
     Must be zero.
     */
    uint32_t reserved;
    /*
     Callee-owned UTF-8 hit text.
     */
    uint8_t *text;
    /*
     Number of hit-text bytes.
     */
    size_t text_len;
    /*
     Larger-is-better result score.
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
     Exact dense squared L2 distance.
     */
    double vector_squared_l2;
    /*
     Exact lexical BM25 score.
     */
    double lexical_bm25;
} ZeTextQueryHit;

/*
 Callee-owned text result; release with `ze_text_query_result_free`.
 */
typedef struct ZeTextQueryResult {
    /*
     Caller-provided structure size.
     */
    uint32_t abi_size;
    /*
     Callee-owned allocation generation; caller initializes zero.
     */
    uint32_t abi_reserved;
    /*
     Callee-owned hit array.
     */
    struct ZeTextQueryHit *hits;
    /*
     Number of initialized hits.
     */
    size_t hit_count;
    /*
     Full bundle pair embedding epoch.
     */
    uint64_t embedding_epoch;
    /*
     Pinned store tokenizer epoch.
     */
    uint64_t tokenizer_epoch;
} ZeTextQueryResult;

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
 The first condition that failed in a conditional write.

 Zeroed (apart from the ABI prefix) unless the call returns
 `ZE_ERR_REVISION_CONFLICT`.
 */
typedef struct ZeRevisionConflict {
    /*
     Caller-provided `sizeof(ZeRevisionConflict)`.
     */
    uint32_t abi_size;
    /*
     Must be zero in ABI v1.
     */
    uint32_t abi_reserved;
    /*
     Batch position of the failed condition.
     */
    uint64_t index;
    /*
     Document whose condition failed.
     */
    struct ZeDocId doc_id;
    /*
     The failed condition's `kind`.
     */
    uint32_t expected_kind;
    /*
     One when a live document exists and `current_revision` is its revision.
     */
    uint32_t has_current;
    /*
     The failed condition's `revision`.
     */
    uint64_t expected_revision;
    /*
     Live revision when `has_current` is one, zero otherwise.
     */
    uint64_t current_revision;
} ZeRevisionConflict;

/*
 Requests documents by stable id in caller order.
 */
typedef struct ZeGetRequest {
    /*
     Caller-provided `sizeof(ZeGetRequest)`.
     */
    uint32_t abi_size;
    /*
     Must be zero in ABI v1.
     */
    uint32_t abi_reserved;
    /*
     Caller-owned document-id array.
     */
    const struct ZeDocId *ids;
    /*
     Number of requested ids; must be nonzero.
     */
    size_t id_count;
    /*
     One to return full-precision vectors.
     */
    uint32_t include_vector;
    /*
     One to return stored UTF-8 text.
     */
    uint32_t include_text;
    /*
     One to return opaque metadata bytes.
     */
    uint32_t include_metadata;
    /*
     One to return typed schema attributes.
     */
    uint32_t include_attributes;
} ZeGetRequest;

/*
 One read-side document slot corresponding to one requested id.
 */
typedef struct ZeStoredDocument {
    /*
     One when the requested document is live, zero for a miss or tombstone.
     */
    uint32_t has_document;
    /*
     Requested stable id, including for a missing document.
     */
    struct ZeDocId doc_id;
    /*
     Live revision, or zero when `has_document` is zero.
     */
    uint64_t revision;
    /*
     Canonical timestamp, or zero when `has_document` is zero.
     */
    int64_t timestamp;
    /*
     Full-precision vector owned by the result arena.
     */
    const float *vector;
    /*
     Scalar count in `vector`.
     */
    size_t vector_len;
    /*
     Stored UTF-8 bytes owned by the result arena.
     */
    const uint8_t *text;
    /*
     Number of `text` bytes.
     */
    size_t text_len;
    /*
     Opaque metadata bytes owned by the result arena.
     */
    const uint8_t *metadata;
    /*
     Number of metadata bytes.
     */
    size_t metadata_len;
    /*
     Typed schema values owned by the result arena.
     */
    const struct ZeAttributeValue *attributes;
    /*
     Number of entries in `attributes`.
     */
    size_t attribute_count;
} ZeStoredDocument;

/*
 Callee-owned documents returned by `ze_get`.
 */
typedef struct ZeGetResult {
    /*
     Caller-provided `sizeof(ZeGetResult)`.
     */
    uint32_t abi_size;
    /*
     Caller sets zero; callee returns an opaque allocation generation.
     */
    uint32_t abi_reserved;
    /*
     One arena-owned entry per requested id.
     */
    struct ZeStoredDocument *documents;
    /*
     Number of entries in `documents`; always the request's `id_count`.
     */
    size_t document_count;
    /*
     Number of entries whose `has_document` is zero.
     */
    size_t missing_count;
    /*
     Store generation pinned for the entire read.
     */
    uint64_t generation;
} ZeGetResult;

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
 Delete-by-filter request for `ze_delete_where`.
 */
typedef struct ZeDeleteWhereRequest {
    /*
     Caller-provided `sizeof(ZeDeleteWhereRequest)`.
     */
    uint32_t abi_size;
    /*
     Must be zero in ABI v1.
     */
    uint32_t abi_reserved;
    /*
     Required caller-owned structured filter; null is rejected.
     */
    const struct ZeFilter *filter;
} ZeDeleteWhereRequest;

/*
 Result of `ze_delete_where`.
 */
typedef struct ZeDeleteWhereReport {
    /*
     Caller-provided `sizeof(ZeDeleteWhereReport)`.
     */
    uint32_t abi_size;
    /*
     Must be zero in ABI v1.
     */
    uint32_t abi_reserved;
    /*
     Number of documents deleted; zero when nothing matched.
     */
    uint64_t deleted_count;
    /*
     Store generation when the call returned; unchanged when nothing
     matched.
     */
    uint64_t generation;
} ZeDeleteWhereReport;

/*
 Atomic delete that commits only when every id's condition holds.
 */
typedef struct ZeConditionalDeleteRequest {
    /*
     Caller-provided `sizeof(ZeConditionalDeleteRequest)`.
     */
    uint32_t abi_size;
    /*
     Must be zero in ABI v1.
     */
    uint32_t abi_reserved;
    /*
     Existing v1 delete request, embedded by value.
     */
    struct ZeDeleteRequest batch;
    /*
     Caller-owned conditions; entry `i` applies to `batch.doc_ids[i]`.
     */
    const struct ZeRevisionCondition *conditions;
    /*
     Must equal `batch.doc_id_count`.
     */
    size_t condition_count;
} ZeConditionalDeleteRequest;

/*
 Opaque generation-tagged cooperative-cancellation handle.
 */
typedef uint64_t ze_cancel_token;

/*
 Ordered, filtered, bounded document-enumeration request.
 */
typedef struct ZeScanRequest {
    /*
     Caller-provided `sizeof(ZeScanRequest)`.
     */
    uint32_t abi_size;
    /*
     Must be zero in ABI v1.
     */
    uint32_t abi_reserved;
    /*
     Continuation generation, or zero to start.
     */
    uint64_t cursor_generation;
    /*
     Immutable segment id, or all zero for the active phase.
     */
    uint8_t cursor_segment_id[16];
    /*
     Source-local row at which to resume.
     */
    uint32_t cursor_next_row;
    /*
     Zero for sealed state or one for active state.
     */
    uint32_t cursor_phase;
    /*
     Maximum number of documents to return; must be nonzero.
     */
    size_t limit;
    /*
     Zero storage, one timestamp ascending, or two timestamp descending.
     */
    int32_t order;
    /*
     One to return full-precision vectors.
     */
    uint32_t include_vector;
    /*
     One to return stored UTF-8 text.
     */
    uint32_t include_text;
    /*
     One to return opaque metadata bytes.
     */
    uint32_t include_metadata;
    /*
     One to return typed schema attributes.
     */
    uint32_t include_attributes;
    /*
     One when `start_ts` and `end_ts` carry a timestamp range.
     */
    uint32_t has_timestamp_range;
    /*
     Inclusive timestamp-range start.
     */
    int64_t start_ts;
    /*
     Exclusive timestamp-range end.
     */
    int64_t end_ts;
    /*
     Optional caller-owned structured filter.
     */
    const struct ZeFilter *filter;
    /*
     Optional generation-tagged cancellation token; zero means absent.
     */
    ze_cancel_token cancel_token;
    /*
     Relative monotonic deadline in nanoseconds; zero means absent.
     */
    uint64_t deadline_ns;
} ZeScanRequest;

/*
 Callee-owned document page returned by `ze_scan`.
 */
typedef struct ZeScanResult {
    /*
     Caller-provided `sizeof(ZeScanResult)`.
     */
    uint32_t abi_size;
    /*
     Caller sets zero; callee returns an opaque allocation generation.
     */
    uint32_t abi_reserved;
    /*
     Arena-owned live documents in requested order.
     */
    struct ZeStoredDocument *documents;
    /*
     Number of entries in `documents`.
     */
    size_t document_count;
    /*
     Store generation pinned for the complete scan call.
     */
    uint64_t generation;
    /*
     One when another page is available.
     */
    uint32_t has_more;
    /*
     Segment id of the next row; all zero for active state or no next row.
     */
    uint8_t next_segment_id[16];
    /*
     Source-local row at which the next page starts.
     */
    uint32_t next_row;
    /*
     Zero for sealed state or one for active state.
     */
    uint32_t next_phase;
} ZeScanResult;

/*
 Scan request that may order by a numeric attribute, for `ze_scan_ordered`.

 `scan.order` accepts the `ze_scan` values (zero storage, one timestamp
 ascending, two timestamp descending) plus three attribute ascending and
 four attribute descending. An attribute order sorts by the declared u64,
 i64 or f64 attribute `order_attribute_id`, with ascending document id as
 the tie breaker; f64 compares numerically and -0.0 equals +0.0; a
 document whose value is missing or NaN sorts after every document with a
 value, in both directions.

 A cursor names the order it was issued under: when
 `scan.cursor_generation` is nonzero, `cursor_order` and
 `cursor_order_attribute_id` must repeat the `scan.order` and
 `order_attribute_id` of the request that returned the cursor, and a
 request with a different order rejects it. Any write between pages
 makes the cursor stale (`ZE_ERR_SCAN_STALE`).
 */
typedef struct ZeScanOrderedRequest {
    /*
     Caller-provided `sizeof(ZeScanOrderedRequest)`.
     */
    uint32_t abi_size;
    /*
     Must be zero in ABI v1.
     */
    uint32_t abi_reserved;
    /*
     Existing scan request embedded by value.
     */
    struct ZeScanRequest scan;
    /*
     Attribute ordered by `scan.order` three or four; zero otherwise.
     */
    uint32_t order_attribute_id;
    /*
     `scan.order` of the request that issued the cursor; zero to start.
     */
    int32_t cursor_order;
    /*
     `order_attribute_id` of the request that issued the cursor; zero to
     start or for a non-attribute cursor order.
     */
    uint32_t cursor_order_attribute_id;
} ZeScanOrderedRequest;

/*
 Counts live documents matching an optional filter and timestamp range.
 */
typedef struct ZeCountRequest {
    /*
     Caller-provided `sizeof(ZeCountRequest)`.
     */
    uint32_t abi_size;
    /*
     Must be zero in ABI v1.
     */
    uint32_t abi_reserved;
    /*
     Optional caller-owned structured filter.
     */
    const struct ZeFilter *filter;
    /*
     One when `start_ts` and `end_ts` carry a timestamp range.
     */
    uint32_t has_timestamp_range;
    /*
     Inclusive timestamp-range start.
     */
    int64_t start_ts;
    /*
     Exclusive timestamp-range end.
     */
    int64_t end_ts;
} ZeCountRequest;

/*
 Scalar result returned by `ze_count`.
 */
typedef struct ZeCountResult {
    /*
     Caller-provided `sizeof(ZeCountResult)`.
     */
    uint32_t abi_size;
    /*
     Must be zero in ABI v1.
     */
    uint32_t abi_reserved;
    /*
     Exact number of matching live rows.
     */
    uint64_t count;
    /*
     Store generation pinned for the complete count.
     */
    uint64_t generation;
} ZeCountResult;

/*
 Counts live documents grouped by one attribute value.
 */
typedef struct ZeCountGroupedRequest {
    /*
     Caller-provided `sizeof(ZeCountGroupedRequest)`.
     */
    uint32_t abi_size;
    /*
     Must be zero in ABI v1.
     */
    uint32_t abi_reserved;
    /*
     Existing count request (filter and timestamp range) embedded by value.
     */
    struct ZeCountRequest count;
    /*
     Schema attribute to group by: U64, I64, DictionaryString or RawString.
     */
    uint32_t group_attribute_id;
    /*
     Must be zero.
     */
    uint32_t reserved;
    /*
     Most distinct values accepted, in `1..=ZE_MAX_COUNT_GROUPS`. More
     distinct values fail the call with `ZE_ERR_BUDGET_EXCEEDED`.
     */
    size_t group_limit;
} ZeCountGroupedRequest;

/*
 One callee-owned group of a grouped count.
 */
typedef struct ZeCountGroup {
    /*
     Group value: `value_type` 1 (U64), 2 (I64) or 5 (string, arena-owned
     bytes); `attribute_id` is the grouped attribute.
     */
    struct ZeAttributeValue value;
    /*
     Matching live documents with this value; never zero.
     */
    uint64_t count;
} ZeCountGroup;

/*
 Callee-owned grouped count; release with `ze_count_grouped_result_free`.
 */
typedef struct ZeCountGroupedResult {
    /*
     Caller-provided `sizeof(ZeCountGroupedResult)`.
     */
    uint32_t abi_size;
    /*
     Caller sets zero; callee returns an opaque allocation generation.
     */
    uint32_t abi_reserved;
    /*
     Arena-owned groups in ascending value order (numeric for integers,
     byte order for strings).
     */
    struct ZeCountGroup *groups;
    /*
     Number of entries in `groups`.
     */
    size_t group_count;
    /*
     Matching live documents whose group attribute is null.
     */
    uint64_t missing_count;
    /*
     All matching live documents: the group counts plus `missing_count`.
     */
    uint64_t count;
    /*
     Store generation pinned for every group.
     */
    uint64_t generation;
} ZeCountGroupedResult;

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
     Caller sets zero; callee returns an opaque allocation generation.
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
 Exact vector search restricted by a required structured filter.
 */
typedef struct ZeSearchFilteredRequest {
    /*
     Caller-provided `sizeof(ZeSearchFilteredRequest)`.
     */
    uint32_t abi_size;
    /*
     Must be zero in ABI v1.
     */
    uint32_t abi_reserved;
    /*
     Existing vector-search request embedded by value.
     */
    struct ZeSearchRequest search;
    /*
     Required caller-owned structured filter.
     */
    const struct ZeFilter *filter;
} ZeSearchFilteredRequest;

/*
 Prepares the lexical assembly and prefix vocabulary for the current generation.
 */
typedef struct ZeWarmLexicalRequest {
    /*
     Caller-provided structure size.
     */
    uint32_t abi_size;
    /*
     Must be zero.
     */
    uint32_t abi_reserved;
    /*
     Optional cancellation token; mutually exclusive with deadline_ns.
     */
    ze_cancel_token cancel_token;
    /*
     Relative deadline in nanoseconds; zero means no deadline.
     */
    uint64_t deadline_ns;
} ZeWarmLexicalRequest;

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
     Lexical query option bits; unknown bits are rejected.
     */
    uint32_t lexical_flags;
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
     Caller sets zero; callee returns an opaque allocation generation.
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
 One half-open matched UTF-8 byte range. `ZeQuerySnippet.highlights` are
 excerpt-relative; `ZeSnippetSourceRanges.highlights` are absolute in source text.
 */
typedef struct ZeSnippetHighlight {
    /*
     Inclusive byte start in the coordinate system of the containing result.
     */
    size_t start;
    /*
     Exclusive byte end in the coordinate system of the containing result.
     */
    size_t end;
} ZeSnippetHighlight;

/*
 One hit's excerpt of its document's stored text, with the ranges the
 query matched. Every offset is a UTF-8 character boundary.
 */
typedef struct ZeQuerySnippet {
    /*
     One when this hit has a snippet. Zero when the document has no stored
     text or its text contains none of the query's matched terms, which
     happens only for a hybrid hit whose `lexical_bm25` is zero. A hit with
     a positive `lexical_bm25` always has a snippet.
     */
    uint32_t has_snippet;
    /*
     One when the excerpt starts after the start of the stored text.
     */
    uint32_t truncated_start;
    /*
     One when the excerpt ends before the end of the stored text.
     */
    uint32_t truncated_end;
    /*
     Always zero.
     */
    uint32_t reserved;
    /*
     Callee-owned UTF-8 excerpt, not NUL-terminated.
     */
    const uint8_t *text;
    /*
     Number of `text` bytes.
     */
    size_t text_len;
    /*
     Callee-owned matched ranges, ascending and non-overlapping.
     */
    const struct ZeSnippetHighlight *highlights;
    /*
     Number of `highlights`.
     */
    size_t highlight_count;
} ZeQuerySnippet;

/*
 Callee-owned snippets aligned one-to-one with a query's hits; release with
 `ze_query_snippets_free`.
 */
typedef struct ZeQuerySnippets {
    /*
     Caller-provided structure size.
     */
    uint32_t abi_size;
    /*
     Caller sets zero; callee returns an opaque allocation generation.
     */
    uint32_t abi_reserved;
    /*
     Callee-owned snippet array, or null when `snippet_count` is zero.
     */
    struct ZeQuerySnippet *snippets;
    /*
     Number of snippets; equals the query result's `hit_count`.
     */
    size_t snippet_count;
} ZeQuerySnippets;

/*
 Eligibility constraints for `ze_query_filtered`; existing query layouts are unchanged.
 */
typedef struct ZeQueryFilter {
    /*
     Caller-provided structure size.
     */
    uint32_t abi_size;
    /*
     Must be zero.
     */
    uint32_t abi_reserved;
    /*
     Optional scan-compatible filter AST.
     */
    const struct ZeFilter *filter;
    /*
     One enables the half-open timestamp range.
     */
    uint32_t has_timestamp_range;
    /*
     Inclusive timestamp start.
     */
    int64_t start_ts;
    /*
     Exclusive timestamp end.
     */
    int64_t end_ts;
} ZeQueryFilter;

/*
 Size-versioned query request with an optional document eligibility set.
 The embedded v1 query layout is unchanged. Caller owns all input buffers.
 */
typedef struct ZeQueryRequestV2 {
    /*
     Caller-provided structure size.
     */
    uint32_t abi_size;
    /*
     Must be zero.
     */
    uint32_t abi_reserved;
    /*
     Existing structured query parameters, with their own v1 size header.
     */
    struct ZeQueryRequest query;
    /*
     Caller-owned ids; order and duplicates do not matter.
     */
    const struct ZeDocId *eligible_ids;
    /*
     Number of ids in eligible_ids.
     */
    size_t eligible_count;
    /*
     Zero means unrestricted; one enables the set, including an empty set.
     */
    uint32_t has_eligible;
    /*
     Must be zero.
     */
    uint32_t reserved;
} ZeQueryRequestV2;

/*
 Absolute half-open UTF-8 byte ranges in the document's stored source text.
 Returned by `ze_query_snippet_source_ranges`; borrowed highlights remain
 valid until `ze_query_snippets_free`. No separate free is needed.
 */
typedef struct ZeSnippetSourceRanges {
    /*
     Caller-provided structure size.
     */
    uint32_t abi_size;
    /*
     Must be zero.
     */
    uint32_t abi_reserved;
    /*
     Inclusive excerpt start in the source text.
     */
    size_t source_start;
    /*
     Exclusive excerpt end in the source text.
     */
    size_t source_end;
    /*
     Absolute source byte ranges, aligned with the snippet's highlights.
     */
    const struct ZeSnippetHighlight *highlights;
    /*
     Number of highlights; zero for a hit without a snippet.
     */
    size_t highlight_count;
} ZeSnippetSourceRanges;

/*
 Immutable changes completed during open. No allocation needs freeing.
 */
typedef struct ZeOpenMigrations {
    /*
     Caller-provided struct size.
     */
    uint32_t abi_size;
    /*
     Must be zero.
     */
    uint32_t abi_reserved;
    /*
     Generation visible at completion of open.
     */
    uint64_t generation;
    /*
     Bit 0: additive schema committed; bit 1: incomplete WAL tail cut.
     */
    uint32_t changes;
    /*
     Manifest format before and after these changes (currently 2).
     */
    uint16_t manifest_version;
    /*
     WAL format before and after these changes (currently 1).
     */
    uint16_t wal_version;
} ZeOpenMigrations;

/*
 A generation returned by seal or idle merge.
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
 Explicit seal or idle-merge cancellation request.
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
 Consistent-snapshot request: the directory `ze_snapshot` writes.
 */
typedef struct ZeSnapshotRequest {
    /*
     Caller-provided `sizeof(ZeSnapshotRequest)`.
     */
    uint32_t abi_size;
    /*
     Must be zero in ABI v1.
     */
    uint32_t abi_reserved;
    /*
     Caller-owned UTF-8 target directory path, without a NUL.
     */
    const uint8_t *target;
    /*
     Number of target-path bytes; must be nonzero.
     */
    size_t target_len;
} ZeSnapshotRequest;

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
 One inspected user attribute, copied into caller-owned storage.
 */
typedef struct ZeSchemaColumnResult {
    /*
     Caller-provided sizeof this structure.
     */
    uint32_t abi_size;
    /*
     Must be zero.
     */
    uint32_t abi_reserved;
    /*
     Total number of user attributes (excludes ts).
     */
    size_t column_count;
    /*
     Required UTF-8 name bytes, without a trailing NUL.
     */
    size_t name_len;
    /*
     Schema-local attribute id; zero when index equals column_count.
     */
    uint32_t attribute_id;
    /*
     Same discriminants as ZeAttributeDefinition.
     */
    int32_t attribute_type;
    /*
     One when nullable.
     */
    uint32_t nullable;
} ZeSchemaColumnResult;

/*
 Store verification request.
 */
typedef struct ZeVerifyRequest {
    /*
     Caller-provided `sizeof(ZeVerifyRequest)`.
     */
    uint32_t abi_size;
    /*
     Must be zero in ABI v1.
     */
    uint32_t abi_reserved;
    /*
     Caller-owned UTF-8 store-directory path, without interior NUL bytes.
     */
    const uint8_t *path;
    /*
     Number of path bytes.
     */
    size_t path_len;
} ZeVerifyRequest;

/*
 One damaged artifact. Every byte is owned by the containing result arena.
 */
typedef struct ZeVerifyFinding {
    /*
     One of the `ZE_VERIFY_*` kinds. Kinds are append-only.
     */
    uint32_t kind;
    /*
     `1` when `offset` is meaningful, otherwise `0`.
     */
    uint32_t has_offset;
    /*
     Byte offset of the damage inside `file`.
     */
    uint64_t offset;
    /*
     UTF-8 file name relative to the store directory.
     */
    const uint8_t *file;
    /*
     Number of file-name bytes.
     */
    size_t file_len;
    /*
     UTF-8 decoder detail.
     */
    const uint8_t *detail;
    /*
     Number of detail bytes.
     */
    size_t detail_len;
} ZeVerifyFinding;

/*
 Callee-owned verification report; release with `ze_verify_result_free`.
 */
typedef struct ZeVerifyResult {
    /*
     Caller-provided structure size.
     */
    uint32_t abi_size;
    /*
     Caller sets zero; callee returns an opaque allocation generation.
     */
    uint32_t abi_reserved;
    /*
     Callee-owned finding array, or null when `finding_count` is zero.
     */
    struct ZeVerifyFinding *findings;
    /*
     Number of findings; zero means no damage was found.
     */
    size_t finding_count;
    /*
     Generation of the decoded manifest, or zero without one.
     */
    uint64_t generation;
    /*
     Segments the manifest references.
     */
    uint64_t segments_checked;
    /*
     WAL records that passed checksum and sequence validation.
     */
    uint64_t wal_records_checked;
} ZeVerifyResult;

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
 Opens or idempotently creates `root/name` with the declared namespace spec.
 Reopening an existing namespace matches attributes by `attribute_id`, in
 any order. Every persisted attribute must be declared with the same name,
 type and nullability. A declared attribute the namespace lacks is added
 when it is nullable: a read-write open commits the addition as one new
 generation before returning, documents written earlier read it as null,
 and sealed data is not rewritten. A read-only open cannot add attributes.
 A removed, changed or non-nullable added attribute, or an addition on a
 read-only open, returns `ZE_ERR_SCHEMA_MISMATCH`; the last-error message
 names the attribute. All request data is caller-owned and need only
 outlive this call. The
 returned handle is an ordinary store handle accepted by every existing
 store function.
 */
ze_error_code ze_namespace_open(const struct ZeNamespaceOpenRequest *request,
                                ze_handle *out_handle);

/*
 Opens a namespace with `0` textDefault, `1` code, or `2` voice tokenization.
 No embedding epoch is required. The profile is persisted as the tokenizer
 epoch; reopening must declare the same profile. If spec.epoch is supplied,
 its tokenizer profile must agree. Existing request layouts are unchanged.
 */
ze_error_code ze_namespace_open_with_tokenizer(const struct ZeNamespaceOpenRequest *request,
                                               int32_t tokenizer_profile,
                                               ze_handle *out_handle);

/*
 Commits all participant mutations with one fully synced root decision.
 Close participating writable handles and ze_open_snapshot views first.
 A view keeps its participant busy even after its source writer closes.
 Upserts, explicit deletes, then filter deletes execute privately per namespace.
 New namespace/direct-path opens select the entire committed local result even
 before any sibling opens; existing readers retain their old snapshot. Read-only
 opens perform no recovery writes. Missing/corrupt root or preparations fail.
 An I/O error at commit has an indeterminate outcome: reopen before retrying.
 Previous stores and abandoned preparations are retained; deletes are logical,
 not a physical-erasure promise. Keep the root intact and use ze_snapshot for
 independent exports. Ordinary writes issue no transaction/root I/O.
 No handle is accepted, so there is no handle poison state for this export.
 */
ze_error_code ze_namespace_batch(const struct ZeNamespaceBatchRequest *request);

/*
 Declares a durable child-to-parent id128 cascade rule under the root lock.
 Participants contain existing specs and no mutations. Close their writers and
 snapshots first. Generations is required but is not written by declaration.
 Cycles fail with ZE_ERR_CASCADE_CYCLE and the namespace cycle in last_error.
 No handle is accepted; this export has no handle poison state.
 */
ze_error_code ze_namespace_declare_cascade(const struct ZeNamespaceBatchRequest *request,
                                           const struct ZeCascadeDeclaration *declaration);

/*
 Atomically deletes explicit IDs and transitive declared dependants.
 Supply 1..128 participants including every reachable namespace; only deletes
 may be populated. Inherits ze_namespace_batch closed-writer, snapshot,
 indeterminate-outcome and logical-delete limits. No handle poison state.
 */
ze_error_code ze_namespace_delete_cascade(const struct ZeNamespaceBatchRequest *request);

/*
 Commits a batch using same-process live writable handles.
 A caught panic poisons all acquired participants. An indeterminate commit
 fences the core writers; reopen before retrying. Generations change only on
 success. All pointers and output storage remain caller-owned.
 */
ze_error_code ze_namespace_batch_live(const struct ZeNamespaceBatchLiveRequest *request);

/*
 Lists direct child directories of `root` that contain a `manifest.ze`, in
 ascending byte order. The returned names share one callee-owned arena.
 */
ze_error_code ze_namespace_list(const struct ZeNamespaceListRequest *request,
                                struct ZeNamespaceListResult *out_result);

/*
 Releases the single arena owned by a namespace-list result. A zeroed result
 is accepted as a successful no-op.
 */
ze_error_code ze_namespace_list_result_free(struct ZeNamespaceListResult *result);

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
 Opens a text store bound to one immutable `.zem` model bundle.
 */
ze_error_code ze_text_open(const struct ZeTextOpenRequest *request,
                           ze_handle *out_handle);

/*
 Ingests text through the bounded tokenizer, MLX, writer, and maintenance pipeline.
 */
ze_error_code ze_text_ingest(ze_handle handle,
                             const struct ZeTextIngestRequest *request,
                             struct ZeMutationReport *out_report);

/*
 Repeats bounded text-store maintenance slices until all due work completes.
 */
ze_error_code ze_text_maintain(ze_handle handle,
                               const struct ZeMaintainRequest *request,
                               struct ZeMaintainReport *out_report);

/*
 Executes one dense, lexical, or hybrid text query and returns stored text on every hit.
 */
ze_error_code ze_text_query(ze_handle handle,
                            const struct ZeTextQueryRequest *request,
                            struct ZeTextQueryResult *out_result);

/*
 Frees a text query result and every callee-owned hit string exactly once.
 */
ze_error_code ze_text_query_result_free(struct ZeTextQueryResult *result);

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
 Atomically ingests caller-owned document records with typed schema
 attributes. Every const pointer is caller-owned and need only outlive the
 call. Not cancellable in v1; the engine offers no token here.
 */
ze_error_code ze_upsert(ze_handle handle,
                        const struct ZeUpsertRequest *request,
                        struct ZeMutationReport *out_report);

/*
 Atomic [`ze_upsert`] that commits only when every document's revision
 condition holds. Conditions are checked by the store's single writer
 against the latest committed state before the batch, atomically with
 applying it. On `ZE_ERR_REVISION_CONFLICT` nothing is written and
 `out_conflict` names the first failed condition; on any other outcome it
 is zeroed. Every const pointer is caller-owned and need only outlive the
 call.
 */
ze_error_code ze_upsert_conditional(ze_handle handle,
                                    const struct ZeConditionalUpsertRequest *request,
                                    struct ZeMutationReport *out_report,
                                    struct ZeRevisionConflict *out_conflict);

/*
 Reads documents by stable id from one pinned generation. Request data is
 caller-owned for the call; the returned arena must be released exactly once
 with [`ze_get_result_free`].
 */
ze_error_code ze_get(ze_handle handle,
                     const struct ZeGetRequest *request,
                     struct ZeGetResult *out_result);

/*
 Releases the single arena owned by a get result.
 */
ze_error_code ze_get_result_free(struct ZeGetResult *result);

/*
 Atomically tombstones caller-owned document identifiers. Every const
 pointer is caller-owned and need only outlive the call. Not cancellable
 in v1; the engine offers no token here.
 */
ze_error_code ze_delete(ze_handle handle,
                        const struct ZeDeleteRequest *request,
                        struct ZeMutationReport *out_report);

/*
 Atomically deletes every live document whose current version matches a
 required filter, then physically removes their bytes from every file in
 the store before returning. `request->filter` is caller-owned for the
 call; null is `ZE_ERR_INVALID_ARGUMENT`. Readers see every matched
 document or none of them. If the process stops during the call, the next
 writable open finishes the removal before it returns. `ZE_ERR_BUSY` means
 a physical purge is already pending; `ZE_ERR_ACCESS_MODE` a read-only
 handle. No match returns a zero count at the unchanged generation. Not
 cancellable in v1.
 */
ze_error_code ze_delete_where(ze_handle handle,
                              const struct ZeDeleteWhereRequest *request,
                              struct ZeDeleteWhereReport *out_report);

/*
 Atomic [`ze_delete`] that commits only when every id's revision
 condition holds, with the same checking and `out_conflict` contract as
 [`ze_upsert_conditional`]. Every const pointer is caller-owned and need
 only outlive the call.
 */
ze_error_code ze_delete_conditional(ze_handle handle,
                                    const struct ZeConditionalDeleteRequest *request,
                                    struct ZeMutationReport *out_report,
                                    struct ZeRevisionConflict *out_conflict);

/*
 Enumerates one ordered, filtered page of live documents. All request and
 filter pointers are caller-owned for the call; the returned arena must be
 released exactly once with [`ze_scan_result_free`].
 */
ze_error_code ze_scan(ze_handle handle,
                      const struct ZeScanRequest *request,
                      struct ZeScanResult *out_result);

/*
 Enumerates one page of live documents like [`ze_scan`], additionally
 ordered by a declared numeric attribute when `scan.order` is three or
 four. The embedded request, filter and cursor contracts match `ze_scan`;
 the cursor fields must name the order that issued the cursor. The returned
 arena must be released exactly once with [`ze_scan_result_free`].
 */
ze_error_code ze_scan_ordered(ze_handle handle,
                              const struct ZeScanOrderedRequest *request,
                              struct ZeScanResult *out_result);

/*
 Counts live documents matching an optional filter and timestamp range.
 All request and filter pointers are caller-owned for the call.
 */
ze_error_code ze_count(ze_handle handle,
                       const struct ZeCountRequest *request,
                       struct ZeCountResult *out_result);

/*
 Counts live documents matching an optional filter and timestamp range,
 grouped by one U64, I64, DictionaryString or RawString attribute. Rows
 whose attribute is null are reported in `missing_count`, not as a
 group. Every group comes from one pinned generation. More distinct
 values than `group_limit` fail with `ZE_ERR_BUDGET_EXCEEDED` and no
 groups; other attribute types or unknown ids fail with
 `ZE_ERR_INVALID_ARGUMENT`. Request and filter pointers are caller-owned
 for the call; release the result exactly once with
 [`ze_count_grouped_result_free`].
 */
ze_error_code ze_count_grouped(ze_handle handle,
                               const struct ZeCountGroupedRequest *request,
                               struct ZeCountGroupedResult *out_result);

/*
 Releases the single arena owned by a grouped count result. A zeroed
 result or a second free of the same result is rejected without effect.
 */
ze_error_code ze_count_grouped_result_free(struct ZeCountGroupedResult *result);

/*
 Releases the single arena owned by a scan result.
 */
ze_error_code ze_scan_result_free(struct ZeScanResult *result);

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
 Searches active and immutable store state through a required structured
 filter. The embedded search request and filter are caller-owned for the
 call. On success `hits` is released with [`ze_search_result_free`].
 */
ze_error_code ze_search_filtered(ze_handle handle,
                                 const struct ZeSearchFilteredRequest *request,
                                 struct ZeSearchResult *out_result);

/*
 Prepares lexical assembly and prefix vocabulary without executing a query.
 Mutations invalidate preparation; callers may warm again afterward.
 */
ze_error_code ze_warm_lexical(ze_handle handle,
                              const struct ZeWarmLexicalRequest *request);

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
 Runs `ze_query` and also returns, for each hit, an excerpt of its stored
 text with the ranges the query matched. The request must carry a lexical
 leg and `snippet_bytes` must be nonzero.

 Matching is the query's own: the store's analyzer re-reads the text pinned
 for the queried generation and marks every token whose analyzed term is
 one the lexical leg scored, so stemmed, folded and prefix-expanded forms
 are marked exactly as they matched. A document has one text field, so a
 hit has at most one snippet.

 The excerpt starts at a matched token and covers `snippet_bytes` bytes,
 extended by at most three bytes to end on a character boundary. Of the
 windows starting at each match, the one covering the most distinct
 matches wins, then the most matches, then the earliest, so snippets are
 deterministic. A highlight is reported only when it lies wholly inside
 the excerpt. No ellipsis is inserted; `truncated_start` and
 `truncated_end` report a cut. Offsets are UTF-8 bytes from the start of
 the excerpt.

 `snippets` holds exactly one entry per hit, in hit order. On success
 release `out_result` with `ze_query_result_free` and `out_snippets` with
 `ze_query_snippets_free`. Once both outputs validate, any failure leaves
 both zeroed. `ze_query` computes no snippet and reads no stored text for a
 flat-term query.
 */
ze_error_code ze_query_with_snippets(ze_handle handle,
                                     const struct ZeQueryRequest *request,
                                     size_t snippet_bytes,
                                     struct ZeQueryResult *out_result,
                                     struct ZeQuerySnippets *out_snippets);

/*
 Runs a query with scan-compatible eligibility constraints before top-k and fusion.
 A zero `snippet_bytes` disables snippets and permits a null `out_snippets`.
 Release results with the existing query and snippet free functions.
 */
ze_error_code ze_query_filtered(ze_handle handle,
                                const struct ZeQueryRequest *request,
                                const struct ZeQueryFilter *constraints,
                                size_t snippet_bytes,
                                struct ZeQueryResult *out_result,
                                struct ZeQuerySnippets *out_snippets);

/*
 Runs a v2 query with optional eligibility, attribute/time filters, and snippets.
 Eligibility is applied to both legs before fusion. A zero snippet_bytes
 permits a null out_snippets. Release outputs with the existing free functions.
 */
ze_error_code ze_query_v2(ze_handle handle,
                          const struct ZeQueryRequestV2 *request,
                          const struct ZeQueryFilter *constraints,
                          size_t snippet_bytes,
                          struct ZeQueryResult *out_result,
                          struct ZeQuerySnippets *out_snippets);

/*
 Returns absolute source byte ranges for one hit from either
 `ze_query_with_snippets` or `ze_query_filtered`, without changing their
 frozen result layouts. `index` must be less than `snippet_count`.
 A hit without a snippet returns zero bounds and no highlights.
 The input must be the unmodified live result; do not free it concurrently.
 Highlight memory is borrowed until `ze_query_snippets_free`.
 Once validated, `out_ranges` is zeroed on failure (preserving `abi_size`).
 */
ze_error_code ze_query_snippet_source_ranges(const struct ZeQuerySnippets *snippets,
                                             size_t index,
                                             struct ZeSnippetSourceRanges *out_ranges);

/*
 Releases callee-owned query snippets; a zeroed value is a successful
 no-op. `snippets` is caller-owned; only its `snippets` allocation, which
 also holds every excerpt and highlight, is released.
 */
ze_error_code ze_query_snippets_free(struct ZeQuerySnippets *snippets);

/*
 Releases a callee-owned query hit array; a zeroed result is a successful
 no-op. `result` is caller-owned; only its `hits` allocation is released.
 */
ze_error_code ze_query_result_free(struct ZeQueryResult *result);

/*
 Returns changes completed by this handle's open; repeated calls return the same report.
 */
ze_error_code ze_open_migrations(ze_handle handle,
                                 struct ZeOpenMigrations *out_report);

/*
 Rebuilds text postings from stored text using the current tokenizer.
 Seals pending writes first. Close and reopen after a publication failure.
 */
ze_error_code ze_reindex_text(ze_handle handle,
                              struct ZeGenerationReport *out_report);

/*
 Seals the active segment. Cancellable through `request.cancel_token`.
 */
ze_error_code ze_seal(ze_handle handle,
                      const struct ZeSealRequest *request,
                      struct ZeGenerationReport *out_report);

/*
 Merges small sealed scan segments during host-selected idle time.
 Uses the existing cancellation request layout. Each batch admits at most
 16 inputs and 8 MiB of input files; active writes and WAL stay unchanged.
 */
ze_error_code ze_merge_sealed(ze_handle handle,
                              const struct ZeSealRequest *request,
                              struct ZeGenerationReport *out_report);

/*
 Opens an in-place read-only handle over the source's current generation.
 No files are copied. Close the returned handle with `ze_close`. It remains
 readable after source close and protects retired files until it is closed.
 The source must be writable. Physical purge returns `ZE_ERR_STORE_BUSY` while a
 view is open; ordinary writes, seals and logical deletes may continue.
 */
ze_error_code ze_open_snapshot(ze_handle handle, ze_handle *out_handle);

/*
 Writes a consistent snapshot of the store into `request.target` and
 reports the generation it captured. The target must not exist or must be
 an empty directory, its parent must exist, and it must not lie inside the
 store; otherwise `ZE_ERR_INVALID_ARGUMENT` and nothing is written. A
 read-only handle returns `ZE_ERR_ACCESS_MODE`, and a pending physical
 purge returns `ZE_ERR_UNSUPPORTED` until it is awaited. This is a reader call:
 writers on other threads are blocked only while the generation is pinned,
 and their later writes are absent from the snapshot. `ze_close` cancels
 an in-flight snapshot (`ZE_ERR_CANCELLED`). A failed snapshot never
 creates the target. The snapshot is an ordinary store directory: open it
 with `ze_open` or `ze_namespace_open`, read-only or read-write, to restore
 the captured state.
 */
ze_error_code ze_snapshot(ze_handle handle,
                          const struct ZeSnapshotRequest *request,
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

/*
 Opens an existing store read-only for diagnostics using its persisted epoch.
 The caller owns path bytes and out_handle; no repair or write is performed.
 */
ze_error_code ze_open_inspection(const uint8_t *path,
                                 size_t path_len,
                                 ze_handle *out_handle);

/*
 Inspects one user attribute by zero-based index. Index equal to column_count
 returns an empty column; larger indices fail. A zero name_capacity probes the
 required name_len. Otherwise name must hold name_capacity writable bytes,
 which must cover name_len. No pointers are retained or returned.
 */
ze_error_code ze_schema_column(ze_handle handle,
                               size_t index,
                               uint8_t *name,
                               size_t name_capacity,
                               struct ZeSchemaColumnResult *out_result);

/*
 Walks the store directory at `request.path` and reports every damaged
 manifest, segment, WAL, and pending purge-intent artifact. The store is never modified: no file
 is created, written, renamed, locked, or removed, so this is safe to run
 after an unclean shutdown and before any open. Damage is reported as
 findings with `ZE_OK`; an error code means the walk could not start
 (`ZE_ERR_NOT_FOUND` for a missing path, `ZE_ERR_IO` for a path that is not
 a directory or cannot be listed). A store another process is writing can
 report a write that is in flight.
 */
ze_error_code ze_verify(const struct ZeVerifyRequest *request,
                        struct ZeVerifyResult *out_result);

/*
 Releases the single arena owned by a verification result. A zeroed result
 is accepted as a successful no-op.
 */
ze_error_code ze_verify_result_free(struct ZeVerifyResult *result);

#ifdef __cplusplus
}  // extern "C"
#endif  // __cplusplus

#endif  /* ZEPPELIN_EMBED_H */
