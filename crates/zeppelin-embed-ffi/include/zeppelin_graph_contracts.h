#ifndef ZEPPELIN_GRAPH_CONTRACTS_H
#define ZEPPELIN_GRAPH_CONTRACTS_H

#include "zeppelin_embed.h"

/*
 Value discriminants. Input fields store u32, not this Rust enum, so unknown
 C values can be rejected safely before interpretation.
 */
enum ZeGraphValueTag
#if defined(__cplusplus) || __STDC_VERSION__ >= 202311L
  : uint32_t
#endif // defined(__cplusplus) || __STDC_VERSION__ >= 202311L
 {
    /*
     Explicit null, never an empty string/list or sentinel entity.
     */
    ZE_GRAPH_VALUE_NULL = 0,
    /*
     Boolean field, exactly zero or one.
     */
    ZE_GRAPH_VALUE_BOOL = 1,
    /*
     Exact signed integer field.
     */
    ZE_GRAPH_VALUE_I64 = 2,
    /*
     Exact IEEE binary64 field; nonfinite scalar values are permitted.
     */
    ZE_GRAPH_VALUE_F64 = 3,
    /*
     UTF-8 byte range in the string arena; embedded NUL is allowed.
     */
    ZE_GRAPH_VALUE_STRING = 4,
    /*
     Index into copied node descriptors; never a forgeable engine reference.
     */
    ZE_GRAPH_VALUE_NODE = 5,
    /*
     Index into copied relationship descriptors.
     */
    ZE_GRAPH_VALUE_RELATIONSHIP = 6,
    /*
     Ordered range of value indices in the child-index arena.
     */
    ZE_GRAPH_VALUE_LIST = 7,
};
#ifndef __cplusplus
#if __STDC_VERSION__ >= 202311L
typedef enum ZeGraphValueTag ZeGraphValueTag;
#else
typedef uint32_t ZeGraphValueTag;
#endif // __STDC_VERSION__ >= 202311L
#endif // __cplusplus

/*
 Exact list representation tag, independent of query equality.
 */
enum ZeGraphListKind
#if defined(__cplusplus) || __STDC_VERSION__ >= 202311L
  : uint32_t
#endif // defined(__cplusplus) || __STDC_VERSION__ >= 202311L
 {
    /*
     General query list; mixed/nested/null elements are permitted within bounds.
     */
    ZE_GRAPH_LIST_QUERY = 0,
    /*
     Homogeneous booleans, including a typed empty list.
     */
    ZE_GRAPH_LIST_BOOL = 1,
    /*
     Homogeneous signed integers, including a typed empty list.
     */
    ZE_GRAPH_LIST_I64 = 2,
    /*
     Homogeneous binary64 values, including a typed empty list.
     */
    ZE_GRAPH_LIST_F64 = 3,
    /*
     Homogeneous UTF-8 strings, including a typed empty list.
     */
    ZE_GRAPH_LIST_STRING = 4,
    /*
     Stored untyped empty-list sentinel; count must be zero.
     */
    ZE_GRAPH_LIST_EMPTY = 5,
};
#ifndef __cplusplus
#if __STDC_VERSION__ >= 202311L
typedef enum ZeGraphListKind ZeGraphListKind;
#else
typedef uint32_t ZeGraphListKind;
#endif // __STDC_VERSION__ >= 202311L
#endif // __cplusplus

/*
 Entity domain, independent of value and operation tags.
 */
enum ZeGraphEntityKind
#if defined(__cplusplus) || __STDC_VERSION__ >= 202311L
  : uint32_t
#endif // defined(__cplusplus) || __STDC_VERSION__ >= 202311L
 {
    /*
     Node domain.
     */
    ZE_GRAPH_ENTITY_NODE = 0,
    /*
     Relationship domain.
     */
    ZE_GRAPH_ENTITY_RELATIONSHIP = 1,
};
#ifndef __cplusplus
#if __STDC_VERSION__ >= 202311L
typedef enum ZeGraphEntityKind ZeGraphEntityKind;
#else
typedef uint32_t ZeGraphEntityKind;
#endif // __STDC_VERSION__ >= 202311L
#endif // __cplusplus

/*
 Endpoint reference for this batch only.
 */
enum ZeGraphEndpointKind
#if defined(__cplusplus) || __STDC_VERSION__ >= 202311L
  : uint32_t
#endif // defined(__cplusplus) || __STDC_VERSION__ >= 202311L
 {
    /*
     Unused descriptor; all payload fields zero.
     */
    ZE_GRAPH_ENDPOINT_UNUSED = 0,
    /*
     Existing nonzero NodeId.
     */
    ZE_GRAPH_ENDPOINT_NODE = 1,
    /*
     Zero-based batch item creating or replaying a node.
     */
    ZE_GRAPH_ENDPOINT_LOCAL = 2,
};
#ifndef __cplusplus
#if __STDC_VERSION__ >= 202311L
typedef enum ZeGraphEndpointKind ZeGraphEndpointKind;
#else
typedef uint32_t ZeGraphEndpointKind;
#endif // __STDC_VERSION__ >= 202311L
#endif // __cplusplus

/*
 Exact structured key lifecycle operation.
 */
enum ZeGraphBatchOperation
#if defined(__cplusplus) || __STDC_VERSION__ >= 202311L
  : uint32_t
#endif // defined(__cplusplus) || __STDC_VERSION__ >= 202311L
 {
    /*
     Expect absence and install full image; exact retry is replayable.
     */
    ZE_GRAPH_BATCH_CREATE = 0,
    /*
     Expect matching incarnation and replace full image.
     */
    ZE_GRAPH_BATCH_PUT = 1,
    /*
     Expect matching incarnation and install deletion fence.
     */
    ZE_GRAPH_BATCH_DELETE = 2,
    /*
     Expect deletion revision; allocate a new incarnation.
     */
    ZE_GRAPH_BATCH_RECREATE = 3,
};
#ifndef __cplusplus
#if __STDC_VERSION__ >= 202311L
typedef enum ZeGraphBatchOperation ZeGraphBatchOperation;
#else
typedef uint32_t ZeGraphBatchOperation;
#endif // __STDC_VERSION__ >= 202311L
#endif // __cplusplus

/*
 Expression tag; operand references are expression indices, never source text.
 */
enum ZeGraphExpressionKind
#if defined(__cplusplus) || __STDC_VERSION__ >= 202311L
  : uint32_t
#endif // defined(__cplusplus) || __STDC_VERSION__ >= 202311L
 {
    /*
     Scalar constant from pool.values; no entity literal.
     */
    ZE_GRAPH_EXPR_LITERAL = 0,
    /*
     Read slot in scope at each use.
     */
    ZE_GRAPH_EXPR_SLOT = 1,
    /*
     Read declared parameter.
     */
    ZE_GRAPH_EXPR_PARAMETER = 2,
    /*
     Unary operation on left.
     */
    ZE_GRAPH_EXPR_UNARY = 3,
    /*
     Binary operation on left/right.
     */
    ZE_GRAPH_EXPR_BINARY = 4,
    /*
     Static named property of left.
     */
    ZE_GRAPH_EXPR_PROPERTY = 5,
    /*
     Static named node label predicate on left.
     */
    ZE_GRAPH_EXPR_HAS_LABEL = 6,
    /*
     Ordered expression-child range.
     */
    ZE_GRAPH_EXPR_LIST = 7,
    /*
     Count or collect; optional left operand with distinct flag.
     */
    ZE_GRAPH_EXPR_AGGREGATE = 8,
};
#ifndef __cplusplus
#if __STDC_VERSION__ >= 202311L
typedef enum ZeGraphExpressionKind ZeGraphExpressionKind;
#else
typedef uint32_t ZeGraphExpressionKind;
#endif // __STDC_VERSION__ >= 202311L
#endif // __cplusplus

/*
 Unary expression semantics; all other tags reject.
 */
enum ZeGraphUnaryOperation
#if defined(__cplusplus) || __STDC_VERSION__ >= 202311L
  : uint32_t
#endif // defined(__cplusplus) || __STDC_VERSION__ >= 202311L
 {
    /*
     Three-valued logical negation.
     */
    ZE_GRAPH_UNARY_NOT = 0,
    /*
     Checked numeric unary plus.
     */
    ZE_GRAPH_UNARY_POSITIVE = 1,
    /*
     Checked numeric negation.
     */
    ZE_GRAPH_UNARY_NEGATE = 2,
    /*
     Exact null test.
     */
    ZE_GRAPH_UNARY_IS_NULL = 3,
    /*
     Exact nonnull test.
     */
    ZE_GRAPH_UNARY_IS_NOT_NULL = 4,
    /*
     Unicode scalar or list count.
     */
    ZE_GRAPH_UNARY_SIZE = 5,
    /*
     Node label-name list.
     */
    ZE_GRAPH_UNARY_LABELS = 6,
    /*
     Relationship type string.
     */
    ZE_GRAPH_UNARY_RELATIONSHIP_TYPE = 7,
    /*
     Optional stored source text.
     */
    ZE_GRAPH_UNARY_STORED_TEXT = 8,
    /*
     Full node identity as 32 lowercase hex digits.
     */
    ZE_GRAPH_UNARY_NODE_ID_TEXT = 9,
    /*
     Full relationship identity as 32 lowercase hex digits.
     */
    ZE_GRAPH_UNARY_RELATIONSHIP_ID_TEXT = 10,
};
#ifndef __cplusplus
#if __STDC_VERSION__ >= 202311L
typedef enum ZeGraphUnaryOperation ZeGraphUnaryOperation;
#else
typedef uint32_t ZeGraphUnaryOperation;
#endif // __STDC_VERSION__ >= 202311L
#endif // __cplusplus

/*
 Binary query semantics; equality is not canonical retry equality.
 */
enum ZeGraphBinaryOperation
#if defined(__cplusplus) || __STDC_VERSION__ >= 202311L
  : uint32_t
#endif // defined(__cplusplus) || __STDC_VERSION__ >= 202311L
 {
    /*
     Three-valued AND.
     */
    ZE_GRAPH_BINARY_AND = 0,
    /*
     Three-valued OR.
     */
    ZE_GRAPH_BINARY_OR = 1,
    /*
     Three-valued XOR.
     */
    ZE_GRAPH_BINARY_XOR = 2,
    /*
     Nullable query equality.
     */
    ZE_GRAPH_BINARY_EQUAL = 3,
    /*
     Nullable query inequality.
     */
    ZE_GRAPH_BINARY_NOT_EQUAL = 4,
    /*
     Nullable less comparison.
     */
    ZE_GRAPH_BINARY_LESS = 5,
    /*
     Nullable less-or-equal comparison.
     */
    ZE_GRAPH_BINARY_LESS_EQUAL = 6,
    /*
     Nullable greater comparison.
     */
    ZE_GRAPH_BINARY_GREATER = 7,
    /*
     Nullable greater-or-equal comparison.
     */
    ZE_GRAPH_BINARY_GREATER_EQUAL = 8,
    /*
     Checked numeric addition.
     */
    ZE_GRAPH_BINARY_ADD = 9,
    /*
     Checked numeric subtraction.
     */
    ZE_GRAPH_BINARY_SUBTRACT = 10,
    /*
     Checked numeric multiplication.
     */
    ZE_GRAPH_BINARY_MULTIPLY = 11,
    /*
     Integer truncating or floating division.
     */
    ZE_GRAPH_BINARY_DIVIDE = 12,
    /*
     Remainder with dividend sign.
     */
    ZE_GRAPH_BINARY_REMAINDER = 13,
    /*
     Exact string prefix.
     */
    ZE_GRAPH_BINARY_STARTS_WITH = 14,
    /*
     Exact string suffix.
     */
    ZE_GRAPH_BINARY_ENDS_WITH = 15,
    /*
     Exact string containment.
     */
    ZE_GRAPH_BINARY_CONTAINS = 16,
    /*
     Three-valued list membership.
     */
    ZE_GRAPH_BINARY_IN = 17,
    /*
     Signed list indexing.
     */
    ZE_GRAPH_BINARY_INDEX = 18,
};
#ifndef __cplusplus
#if __STDC_VERSION__ >= 202311L
typedef enum ZeGraphBinaryOperation ZeGraphBinaryOperation;
#else
typedef uint32_t ZeGraphBinaryOperation;
#endif // __STDC_VERSION__ >= 202311L
#endif // __cplusplus

/*
 Top-level aggregate forms only; nested or incorrect-context uses reject.
 */
enum ZeGraphAggregateOperation
#if defined(__cplusplus) || __STDC_VERSION__ >= 202311L
  : uint32_t
#endif // defined(__cplusplus) || __STDC_VERSION__ >= 202311L
 {
    /*
     Count rows when has_operand=0; otherwise nonnull values.
     */
    ZE_GRAPH_AGGREGATE_COUNT = 0,
    /*
     Ordered nonnull values; has_operand must be one.
     */
    ZE_GRAPH_AGGREGATE_COLLECT = 1,
};
#ifndef __cplusplus
#if __STDC_VERSION__ >= 202311L
typedef enum ZeGraphAggregateOperation ZeGraphAggregateOperation;
#else
typedef uint32_t ZeGraphAggregateOperation;
#endif // __STDC_VERSION__ >= 202311L
#endif // __cplusplus

/*
 Search tier. Absent preference remains distinct from explicit Auto.
 */
enum ZeGraphTier
#if defined(__cplusplus) || __STDC_VERSION__ >= 202311L
  : uint32_t
#endif // defined(__cplusplus) || __STDC_VERSION__ >= 202311L
 {
    /*
     Explicit automatic tier.
     */
    ZE_GRAPH_TIER_AUTO = 0,
    /*
     Exhaustive original-f32 scoring.
     */
    ZE_GRAPH_TIER_EXACT = 1,
    /*
     Quantized scan; coverage and precision are reported separately.
     */
    ZE_GRAPH_TIER_SCAN = 2,
    /*
     Explicit structured ANN tier; not a textual approximate Cypher mode.
     */
    ZE_GRAPH_TIER_GRAPH = 3,
};
#ifndef __cplusplus
#if __STDC_VERSION__ >= 202311L
typedef enum ZeGraphTier ZeGraphTier;
#else
typedef uint32_t ZeGraphTier;
#endif // __STDC_VERSION__ >= 202311L
#endif // __cplusplus

/*
 Once-per-query search source.
 */
enum ZeGraphSearchKind
#if defined(__cplusplus) || __STDC_VERSION__ >= 202311L
  : uint32_t
#endif // defined(__cplusplus) || __STDC_VERSION__ >= 202311L
 {
    /*
     Vector distance ascending.
     */
    ZE_GRAPH_SEARCH_VECTOR = 0,
    /*
     Lexical BM25 descending.
     */
    ZE_GRAPH_SEARCH_TEXT = 1,
    /*
     Fused score descending, independent nullable component yields.
     */
    ZE_GRAPH_SEARCH_HYBRID = 2,
};
#ifndef __cplusplus
#if __STDC_VERSION__ >= 202311L
typedef enum ZeGraphSearchKind ZeGraphSearchKind;
#else
typedef uint32_t ZeGraphSearchKind;
#endif // __STDC_VERSION__ >= 202311L
#endif // __cplusplus

/*
 Typed graph operators; no SQL/Cypher text or serialized opcode stream.
 */
enum ZeGraphOperatorKind
#if defined(__cplusplus) || __STDC_VERSION__ >= 202311L
  : uint32_t
#endif // defined(__cplusplus) || __STDC_VERSION__ >= 202311L
 {
    /*
     Zero inputs, one empty row.
     */
    ZE_GRAPH_OP_UNIT = 0,
    /*
     Two inputs, optional combined-scope predicate; shared slots are equality keys.
     */
    ZE_GRAPH_OP_JOIN = 1,
    /*
     One input, query-equivalence whole-row deduplication.
     */
    ZE_GRAPH_OP_DISTINCT = 2,
    /*
     One input, sort_keys range.
     */
    ZE_GRAPH_OP_SORT = 3,
    /*
     Zero inputs, node_slot and optional label name.
     */
    ZE_GRAPH_OP_SCAN_NODES = 4,
    /*
     One input, drain/freeze full mutation input.
     */
    ZE_GRAPH_OP_EAGER = 5,
    /*
     One immediate Eager input, ordered mutations range.
     */
    ZE_GRAPH_OP_MUTATE = 6,
    /*
     One input, projections group keys and aggregates output range.
     */
    ZE_GRAPH_OP_AGGREGATE = 7,
    /*
     Zero row inputs, search descriptor index; eligibility dependency is explicit.
     */
    ZE_GRAPH_OP_SEARCH = 8,
    /*
     One input, u64 offset and optional limit.
     */
    ZE_GRAPH_OP_OFFSET_LIMIT = 9,
    /*
     Zero inputs, node_slot and nonzero node_id.
     */
    ZE_GRAPH_OP_LOOKUP_NODE = 10,
    /*
     Zero inputs, relationship_slot and nonzero relationship_id.
     */
    ZE_GRAPH_OP_LOOKUP_RELATIONSHIP = 11,
    /*
     Zero inputs, node_slot output, entity_kind, namespace name and key expression.
     */
    ZE_GRAPH_OP_LOOKUP_KEY = 12,
    /*
     One input, source/node/relationship slots, direction, OR type range and pattern.
     */
    ZE_GRAPH_OP_EXPAND = 13,
    /*
     One input, Expand fields plus inclusive path_min/path_max and optional per-edge predicate.
     */
    ZE_GRAPH_OP_BOUNDED_EXPAND = 14,
    /*
     Two inputs, correlated optional predicate before null extension.
     */
    ZE_GRAPH_OP_OPTIONAL_APPLY = 15,
    /*
     One input, projections range.
     */
    ZE_GRAPH_OP_PROJECT = 16,
    /*
     One input, projections and mandatory scope barrier.
     */
    ZE_GRAPH_OP_WITH = 17,
    /*
     One input, required predicate.
     */
    ZE_GRAPH_OP_FILTER = 18,
    /*
     One input, completed output boundary.
     */
    ZE_GRAPH_OP_COLLECT = 19,
    /*
     One input, source_slot node values to immutable deduplicated same-view set_slot; cardinality <=524288.
     */
    ZE_GRAPH_OP_ELIGIBLE_SET = 20,
};
#ifndef __cplusplus
#if __STDC_VERSION__ >= 202311L
typedef enum ZeGraphOperatorKind ZeGraphOperatorKind;
#else
typedef uint32_t ZeGraphOperatorKind;
#endif // __STDC_VERSION__ >= 202311L
#endif // __cplusplus

/*
 Ordered structured query mutation items; evaluated in progressive overlay.
 */
enum ZeGraphMutationKind
#if defined(__cplusplus) || __STDC_VERSION__ >= 202311L
  : uint32_t
#endif // defined(__cplusplus) || __STDC_VERSION__ >= 202311L
 {
    /*
     Fresh node bound to output with label set.
     */
    ZE_GRAPH_MUTATION_CREATE_NODE = 0,
    /*
     Fresh relationship bound to output with source/target expressions and fixed type name.
     */
    ZE_GRAPH_MUTATION_CREATE_RELATIONSHIP = 1,
    /*
     Remove named property from entity.
     */
    ZE_GRAPH_MUTATION_REMOVE_PROPERTY = 2,
    /*
     Add/remove named label using present.
     */
    ZE_GRAPH_MUTATION_SET_LABEL = 3,
    /*
     Delete entity using detach for nodes only.
     */
    ZE_GRAPH_MUTATION_DELETE = 4,
    /*
     Assign named value expression; null removes property.
     */
    ZE_GRAPH_MUTATION_SET_PROPERTY = 5,
};
#ifndef __cplusplus
#if __STDC_VERSION__ >= 202311L
typedef enum ZeGraphMutationKind ZeGraphMutationKind;
#else
typedef uint32_t ZeGraphMutationKind;
#endif // __STDC_VERSION__ >= 202311L
#endif // __cplusplus

/*
 Explicit graph create/open distinction; no legacy fallback or weaker durability.
 */
enum ZeGraphOpenMode
#if defined(__cplusplus) || __STDC_VERSION__ >= 202311L
  : uint32_t
#endif // defined(__cplusplus) || __STDC_VERSION__ >= 202311L
 {
    /*
     Create a fresh native graph store; never replace incompatible or existing contents.
     */
    ZE_GRAPH_OPEN_CREATE = 0,
    /*
     Open existing native graph with exclusive writer lock and Full durability.
     */
    ZE_GRAPH_OPEN_READ_WRITE = 1,
    /*
     Open existing native graph with shared lock and no recovery mutation.
     */
    ZE_GRAPH_OPEN_READ_ONLY = 2,
};
#ifndef __cplusplus
#if __STDC_VERSION__ >= 202311L
typedef enum ZeGraphOpenMode ZeGraphOpenMode;
#else
typedef uint32_t ZeGraphOpenMode;
#endif // __STDC_VERSION__ >= 202311L
#endif // __cplusplus

/*
 Coordinator-owned durable outcome, also present on errors when output descriptor is valid.
 */
enum ZeGraphDisposition
#if defined(__cplusplus) || __STDC_VERSION__ >= 202311L
  : uint32_t
#endif // defined(__cplusplus) || __STDC_VERSION__ >= 202311L
 {
    /*
     Read-only operation; no write attempt.
     */
    ZE_GRAPH_DISPOSITION_NOT_APPLICABLE = 0,
    /*
     Coordinator established definite no durable effects.
     */
    ZE_GRAPH_DISPOSITION_NOT_COMMITTED = 1,
    /*
     Known durable commit; changed generation present.
     */
    ZE_GRAPH_DISPOSITION_COMMITTED = 2,
    /*
     All operations replay; no new changed generation, original per-item generations retained.
     */
    ZE_GRAPH_DISPOSITION_REPLAYED = 3,
    /*
     Successful no durable change, including empty matched input; admitted generation present.
     */
    ZE_GRAPH_DISPOSITION_NO_OP = 4,
    /*
     Durable outcome not established; no guessed new IDs or changed generation.
     */
    ZE_GRAPH_DISPOSITION_INDETERMINATE = 5,
};
#ifndef __cplusplus
#if __STDC_VERSION__ >= 202311L
typedef enum ZeGraphDisposition ZeGraphDisposition;
#else
typedef uint32_t ZeGraphDisposition;
#endif // __STDC_VERSION__ >= 202311L
#endif // __cplusplus

/*
 Precision of computed scores, independent of candidate coverage.
 */
enum ZeGraphScorePrecision
#if defined(__cplusplus) || __STDC_VERSION__ >= 202311L
  : uint32_t
#endif // defined(__cplusplus) || __STDC_VERSION__ >= 202311L
 {
    /*
     No vector component evaluated (for example lexical only).
     */
    ZE_GRAPH_PRECISION_NOT_APPLICABLE = 0,
    /*
     Original f32 vector domain; does not certify candidate coverage.
     */
    ZE_GRAPH_PRECISION_ORIGINAL = 1,
    /*
     Quantized scores without full original rescore.
     */
    ZE_GRAPH_PRECISION_QUANTIZED = 2,
    /*
     Different actual vector scoring precisions are retained.
     */
    ZE_GRAPH_PRECISION_MIXED = 3,
};
#ifndef __cplusplus
#if __STDC_VERSION__ >= 202311L
typedef enum ZeGraphScorePrecision ZeGraphScorePrecision;
#else
typedef uint32_t ZeGraphScorePrecision;
#endif // __STDC_VERSION__ >= 202311L
#endif // __cplusplus

/*
 Candidate coverage evidence; graph expansion cannot upgrade approximate seeds.
 */
enum ZeGraphCandidateCoverage
#if defined(__cplusplus) || __STDC_VERSION__ >= 202311L
  : uint32_t
#endif // defined(__cplusplus) || __STDC_VERSION__ >= 202311L
 {
    /*
     Exhaustive or valid certified eligible top-k.
     */
    ZE_GRAPH_COVERAGE_EXACT = 0,
    /*
     Non-exhaustive candidate source without complete certificate.
     */
    ZE_GRAPH_COVERAGE_APPROXIMATE = 1,
};
#ifndef __cplusplus
#if __STDC_VERSION__ >= 202311L
typedef enum ZeGraphCandidateCoverage ZeGraphCandidateCoverage;
#else
typedef uint32_t ZeGraphCandidateCoverage;
#endif // __STDC_VERSION__ >= 202311L
#endif // __cplusplus

/*
 Proven empty-leg reasons, independent of an empty approximate candidate window.
 */
enum ZeGraphLegState
#if defined(__cplusplus) || __STDC_VERSION__ >= 202311L
  : uint32_t
#endif // defined(__cplusplus) || __STDC_VERSION__ >= 202311L
 {
    /*
     Modality not requested.
     */
    ZE_GRAPH_LEG_NOT_REQUESTED = 0,
    /*
     Eligible modality has candidates/matches.
     */
    ZE_GRAPH_LEG_NONEMPTY = 1,
    /*
     No live indexed modality population.
     */
    ZE_GRAPH_LEG_NO_INDEXED_POPULATION = 2,
    /*
     Indexed population exists but eligibility intersection is empty.
     */
    ZE_GRAPH_LEG_NO_ELIGIBLE_MEMBERS = 3,
    /*
     Eligible indexed text exists but lexical query has no matches.
     */
    ZE_GRAPH_LEG_NO_QUERY_MATCHES = 4,
};
#ifndef __cplusplus
#if __STDC_VERSION__ >= 202311L
typedef enum ZeGraphLegState ZeGraphLegState;
#else
typedef uint32_t ZeGraphLegState;
#endif // __STDC_VERSION__ >= 202311L
#endif // __cplusplus

/*
 Actual cumulative work categories, matching runtime ownership; no TCK side-effect counters.
 */
enum ZeGraphWorkKind
#if defined(__cplusplus) || __STDC_VERSION__ >= 202311L
  : uint32_t
#endif // defined(__cplusplus) || __STDC_VERSION__ >= 202311L
 {
    /*
     Actual OperatorRows under the runtime counter contract.
     */
    ZE_GRAPH_WORK_OPERATOR_ROWS = 0,
    /*
     Actual AdjacencyEntries under the runtime counter contract.
     */
    ZE_GRAPH_WORK_ADJACENCY_ENTRIES = 1,
    /*
     Actual Expressions under the runtime counter contract.
     */
    ZE_GRAPH_WORK_EXPRESSIONS = 2,
    /*
     Actual HashProbes under the runtime counter contract.
     */
    ZE_GRAPH_WORK_HASH_PROBES = 3,
    /*
     Actual CompletedRows under the runtime counter contract.
     */
    ZE_GRAPH_WORK_COMPLETED_ROWS = 4,
    /*
     Actual CompletedBytes under the runtime counter contract.
     */
    ZE_GRAPH_WORK_COMPLETED_BYTES = 5,
    /*
     Actual PreparedPayloadBytes under the runtime counter contract.
     */
    ZE_GRAPH_WORK_PREPARED_PAYLOAD_BYTES = 6,
    /*
     Actual CompletedAbiBytes under the runtime counter contract.
     */
    ZE_GRAPH_WORK_COMPLETED_ABI_BYTES = 7,
    /*
     Actual VectorCoordinates under the runtime counter contract.
     */
    ZE_GRAPH_WORK_VECTOR_COORDINATES = 8,
    /*
     Actual VectorBytes under the runtime counter contract.
     */
    ZE_GRAPH_WORK_VECTOR_BYTES = 9,
    /*
     Actual LexicalPostings under the runtime counter contract.
     */
    ZE_GRAPH_WORK_LEXICAL_POSTINGS = 10,
    /*
     Actual LexicalBlocks under the runtime counter contract.
     */
    ZE_GRAPH_WORK_LEXICAL_BLOCKS = 11,
    /*
     Actual SearchInvocations under the runtime counter contract.
     */
    ZE_GRAPH_WORK_SEARCH_INVOCATIONS = 12,
    /*
     Actual Lookups under the runtime counter contract.
     */
    ZE_GRAPH_WORK_LOOKUPS = 13,
    /*
     Actual Scans under the runtime counter contract.
     */
    ZE_GRAPH_WORK_SCANS = 14,
    /*
     Actual Paths under the runtime counter contract.
     */
    ZE_GRAPH_WORK_PATHS = 15,
    /*
     Actual RowsIn under the runtime counter contract.
     */
    ZE_GRAPH_WORK_ROWS_IN = 16,
    /*
     Actual RowsOut under the runtime counter contract.
     */
    ZE_GRAPH_WORK_ROWS_OUT = 17,
    /*
     Actual JoinProbes under the runtime counter contract.
     */
    ZE_GRAPH_WORK_JOIN_PROBES = 18,
    /*
     Actual GroupKeys under the runtime counter contract.
     */
    ZE_GRAPH_WORK_GROUP_KEYS = 19,
    /*
     Actual EligibilityEntries under the runtime counter contract.
     */
    ZE_GRAPH_WORK_ELIGIBILITY_ENTRIES = 20,
    /*
     Actual CopiedBytes under the runtime counter contract.
     */
    ZE_GRAPH_WORK_COPIED_BYTES = 21,
    /*
     Monotone peak actual charged capacity.
     */
    ZE_GRAPH_WORK_PEAK_OWNED_BYTES = 22,
};
#ifndef __cplusplus
#if __STDC_VERSION__ >= 202311L
typedef enum ZeGraphWorkKind ZeGraphWorkKind;
#else
typedef uint32_t ZeGraphWorkKind;
#endif // __STDC_VERSION__ >= 202311L
#endif // __cplusplus

/*
 Fixed caller-owned byte span; length is bytes, not NUL termination.
 */
typedef struct ZeGraphBytes {
    /*
     Accessible bytes for the synchronous call; null only at count zero.
     */
    const uint8_t *data;
    /*
     Accessible byte count; UTF-8/domain rules depend on the named field.
     */
    size_t count;
} ZeGraphBytes;

/*
 Cooperative request controls; existing cancellation token registry is reused.
 */
typedef struct ZeGraphControl {
    /*
     Exact sizeof this version-one descriptor; fixed array stride.
     */
    uint32_t abi_size;
    /*
     Must be zero.
     */
    uint32_t abi_reserved;
    /*
     Zero means no explicit token; nonzero must name a live cancellation token.
     */
    uint64_t cancel_token;
    /*
     Relative monotonic deadline from call entry; zero means absent.
     */
    uint64_t deadline_ns;
} ZeGraphControl;

/*
 Graph-only open declaration. Document-tower identity is persisted; query tower/alignment is intentionally absent. ZE-69/coordinator own locks, format admission and initialization.
 */
typedef struct ZeGraphOpenRequest {
    /*
     Exact sizeof this version-one descriptor; fixed array stride.
     */
    uint32_t abi_size;
    /*
     Must be zero.
     */
    uint32_t abi_reserved;
    /*
     Nonempty UTF-8 filesystem path; embedded NUL forbidden.
     */
    struct ZeGraphBytes path;
    /*
     One ZeGraphOpenMode.
     */
    uint32_t mode;
    /*
     0 existing general-purpose tokenizer; unknown profiles reject.
     */
    uint32_t tokenizer_profile;
    /*
     Null declares graph without vector space; otherwise exact optional document interpretation.
     */
    const ZeEmbeddingTower *document_tower;
    /*
     Close drain grace period in milliseconds, using existing lifecycle semantics.
     */
    uint64_t reader_drain_timeout_ms;
    /*
     Nonzero shared store owned-capacity ceiling, at most 256 MiB; includes concurrent graph work.
     */
    uint64_t max_resident_bytes;
    /*
     Optional cancellation/deadline for bounded create/open work.
     */
    const struct ZeGraphControl *control;
} ZeGraphOpenRequest;

/*
 Opaque graph-only generation-tagged handle. Zero is never a live handle.
 */
typedef struct ZeGraphHandle {
    /*
     The handle registry owns interpretation; this is not a store pointer.
     */
    uint64_t token;
} ZeGraphHandle;

/*
 Fixed pool span. `start` and `count` use the named target array's elements.
 Checked addition and full containment are required before dereferencing.
 */
typedef struct ZeGraphRange {
    /*
     Zero-based first element; zero for an unused range.
     */
    uint32_t start;
    /*
     Number of elements; an empty range remains distinct from absent data.
     */
    uint32_t count;
} ZeGraphRange;

/*
 Store-local nonzero node identity. No relationship or document conversion.
 */
typedef struct ZeNodeId {
    /*
     Most significant 64 bits.
     */
    uint64_t high;
    /*
     Least significant 64 bits.
     */
    uint64_t low;
} ZeNodeId;

/*
 Store-local nonzero relationship identity, distinct from a node identity.
 */
typedef struct ZeRelId {
    /*
     Most significant 64 bits.
     */
    uint64_t high;
    /*
     Least significant 64 bits.
     */
    uint64_t low;
} ZeRelId;

/*
 Tagged existing/local node endpoint. Inactive fields zero; kind Unused is only legal in non-relationship or delete items.
 */
typedef struct ZeGraphEndpoint {
    /*
     Exact sizeof this version-one descriptor; fixed array stride.
     */
    uint32_t abi_size;
    /*
     Must be zero.
     */
    uint32_t abi_reserved;
    /*
     One ZeGraphEndpointKind value.
     */
    uint32_t kind;
    /*
     Batch item index for Local; zero otherwise.
     */
    uint32_t local_item;
    /*
     Nonzero NodeId for Node; zero otherwise.
     */
    struct ZeNodeId node;
} ZeGraphEndpoint;

/*
 One keyed full-record operation, at most 16384 per atomic batch. No caller-selected fresh identity. Input image optional only for Delete; relationship endpoints are required for nondelete relationship items.
 */
typedef struct ZeGraphBatchItem {
    /*
     Exact sizeof this version-one descriptor; fixed array stride.
     */
    uint32_t abi_size;
    /*
     Must be zero.
     */
    uint32_t abi_reserved;
    /*
     One ZeGraphEntityKind.
     */
    uint32_t entity_kind;
    /*
     One ZeGraphBatchOperation.
     */
    uint32_t operation;
    /*
     Byte-exact application namespace (empty/NUL permitted) in request pool.bytes.
     */
    struct ZeGraphRange namespace_name;
    /*
     Byte-exact application key (empty/NUL permitted) in request pool.bytes.
     */
    struct ZeGraphRange key;
    /*
     Requested positive revision; checked against existing state.
     */
    uint64_t revision;
    /*
     Only node Put/Delete: expected nonzero incarnation; zero otherwise.
     */
    struct ZeNodeId expected_node;
    /*
     Only relationship Put/Delete: expected nonzero incarnation; zero otherwise.
     */
    struct ZeRelId expected_relationship;
    /*
     Positive only for Recreate; zero otherwise.
     */
    uint64_t expected_deletion_revision;
    /*
     Delete only: 0 Restrict, 1 Detach (nodes only); zero otherwise.
     */
    uint32_t delete_mode;
    /*
     Exactly 1 for Create/Put/Recreate, 0 for Delete.
     */
    uint32_t has_image;
    /*
     Index into pool.nodes or pool.relationships by entity_kind; zero if absent.
     */
    uint32_t image;
    /*
     Must be zero.
     */
    uint32_t reserved;
    /*
     Relationship source input; unused for nodes and deletes.
     */
    struct ZeGraphEndpoint source;
    /*
     Relationship target input; unused for nodes and deletes.
     */
    struct ZeGraphEndpoint target;
} ZeGraphBatchItem;

/*
 Version-one flat value descriptor. `abi_size` must equal sizeof this type.
 Reserved and inactive fields are zero (including the bits of inactive F64).
 Only List uses list_kind; String/List use range; Node/Relationship use
 entity_index; each scalar uses its matching named field. Array bounds,
 UTF-8, graph ownership and descendant geometry require the later marshaller.
 */
typedef struct ZeGraphValue {
    /*
     Exact version-one descriptor size; fixed array stride.
     */
    uint32_t abi_size;
    /*
     Must be zero.
     */
    uint32_t abi_reserved;
    /*
     One ZeGraphValueTag discriminant; unknown tags reject.
     */
    uint32_t tag;
    /*
     One ZeGraphListKind for List; zero for all other tags.
     */
    uint32_t list_kind;
    /*
     Zero or one for Bool; zero otherwise.
     */
    uint32_t boolean;
    /*
     Copied Node/Relationship array index; zero for other tags.
     */
    uint32_t entity_index;
    /*
     Exact I64 value; zero otherwise.
     */
    int64_t integer;
    /*
     Exact F64 bits; positive-zero bits otherwise.
     */
    double floating;
    /*
     String bytes or List child-index elements; zero range otherwise.
     */
    struct ZeGraphRange range;
} ZeGraphValue;

/*
 Copied node record or full node input image. Input image identity and metadata are zero; key/revision come from the batch item. Output payload fields require explicit selection.
 */
typedef struct ZeGraphNode {
    /*
     Exact sizeof this version-one descriptor; fixed array stride.
     */
    uint32_t abi_size;
    /*
     Must be zero.
     */
    uint32_t abi_reserved;
    /*
     Full output identity; zero in input images.
     */
    struct ZeNodeId id;
    /*
     Exactly 0 or 1; absence requires zero namespace/key ranges.
     */
    uint32_t has_key;
    /*
     Exactly 0 or 1, independent of text length; absent range is zero.
     */
    uint32_t has_text;
    /*
     Exactly 0 or 1, independent of vector length; absent range is zero.
     */
    uint32_t has_vector;
    /*
     Must be zero.
     */
    uint32_t reserved;
    /*
     Application-key namespace bytes; absent for unkeyed entities.
     */
    struct ZeGraphRange namespace_name;
    /*
     Application-key bytes, not an implicit user property.
     */
    struct ZeGraphRange key;
    /*
     Current positive revision on outputs; zero in fresh input images.
     */
    uint64_t revision;
    /*
     Original last-change generation on outputs; zero in input images.
     */
    uint64_t last_change_generation;
    /*
     Range of pool.properties entries.
     */
    struct ZeGraphRange properties;
    /*
     Optional source text bytes in pool.bytes.
     */
    struct ZeGraphRange text;
    /*
     Optional original finite f32 coordinates in pool.vectors.
     */
    struct ZeGraphRange vector;
    /*
     Range of pool.names UTF-8 label ranges; interpreted as a set.
     */
    struct ZeGraphRange labels;
} ZeGraphNode;

/*
 Copied relationship or full input image. Input IDs/endpoints/key/revision metadata are zero; batch item endpoints and precondition own those fields. Type is always present.
 */
typedef struct ZeGraphRelationship {
    /*
     Exact sizeof this version-one descriptor; fixed array stride.
     */
    uint32_t abi_size;
    /*
     Must be zero.
     */
    uint32_t abi_reserved;
    /*
     Full output identity; zero in input images.
     */
    struct ZeRelId id;
    /*
     Fixed source endpoint on outputs; zero in input images.
     */
    struct ZeNodeId source;
    /*
     Fixed target endpoint on outputs; zero in input images.
     */
    struct ZeNodeId target;
    /*
     Exactly 0 or 1; absence requires zero namespace/key ranges.
     */
    uint32_t has_key;
    /*
     Must be zero.
     */
    uint32_t reserved;
    /*
     Application-key namespace bytes; absent for unkeyed entities.
     */
    struct ZeGraphRange namespace_name;
    /*
     Application-key bytes, not an implicit user property.
     */
    struct ZeGraphRange key;
    /*
     Current positive revision on outputs; zero in fresh input images.
     */
    uint64_t revision;
    /*
     Original last-change generation on outputs; zero in input images.
     */
    uint64_t last_change_generation;
    /*
     Range of pool.properties entries.
     */
    struct ZeGraphRange properties;
    /*
     Exactly one byte-exact relationship-type name (empty/NUL permitted) in pool.bytes.
     */
    struct ZeGraphRange relationship_type;
} ZeGraphRelationship;

/*
 Property entry. Input names are unique; null is not a stored value.
 */
typedef struct ZeGraphProperty {
    /*
     Exact sizeof this version-one descriptor; fixed array stride.
     */
    uint32_t abi_size;
    /*
     Must be zero.
     */
    uint32_t abi_reserved;
    /*
     UTF-8 name bytes in the value pool.
     */
    struct ZeGraphRange name;
    /*
     Index into pool.values.
     */
    uint32_t value;
    /*
     Must be zero.
     */
    uint32_t reserved;
} ZeGraphProperty;

/*
 Flat borrowed input or immutable owned output pool. Every pointer/count and range must be validated before traversal; ownership is supplied by the later adapter.
 */
typedef struct ZeGraphValuePool {
    /*
     Exact sizeof this version-one descriptor; fixed array stride.
     */
    uint32_t abi_size;
    /*
     Must be zero.
     */
    uint32_t abi_reserved;
    /*
     Flat scalar/list/entity descriptors. Null only at zero count.
     */
    const struct ZeGraphValue *values;
    /*
     Accessible elements in values; checked multiplication required.
     */
    size_t value_count;
    /*
     List child indices into values; order and multiplicity preserved. Null only at zero count.
     */
    const uint32_t *children;
    /*
     Accessible elements in children; checked multiplication required.
     */
    size_t child_count;
    /*
     UTF-8 bytes; strings may contain NUL except where a domain forbids it. Null only at zero count.
     */
    const uint8_t *bytes;
    /*
     Accessible elements in bytes; checked multiplication required.
     */
    size_t byte_count;
    /*
     Copied nodes or batch input images; never runtime references. Null only at zero count.
     */
    const struct ZeGraphNode *nodes;
    /*
     Accessible elements in nodes; checked multiplication required.
     */
    size_t node_count;
    /*
     Copied relationships or batch input images. Null only at zero count.
     */
    const struct ZeGraphRelationship *relationships;
    /*
     Accessible elements in relationships; checked multiplication required.
     */
    size_t relationship_count;
    /*
     Property map entries. Null only at zero count.
     */
    const struct ZeGraphProperty *properties;
    /*
     Accessible elements in properties; checked multiplication required.
     */
    size_t property_count;
    /*
     Name ranges into bytes; labels/types use their named domain rules. Null only at zero count.
     */
    const struct ZeGraphRange *names;
    /*
     Accessible elements in names; checked multiplication required.
     */
    size_t name_count;
    /*
     Original finite vector coordinates; scalar F64 remains unrestricted. Null only at zero count.
     */
    const float *vectors;
    /*
     Accessible elements in vectors; checked multiplication required.
     */
    size_t vector_count;
} ZeGraphValuePool;

/*
 One synchronous atomic structured batch. Total canonical input including framing is at most 8 MiB; runtime/staging validates ownership and exact replay.
 */
typedef struct ZeGraphBatchRequest {
    /*
     Exact sizeof this version-one descriptor; fixed array stride.
     */
    uint32_t abi_size;
    /*
     Must be zero.
     */
    uint32_t abi_reserved;
    /*
     Nonempty readable fixed-stride items.
     */
    const struct ZeGraphBatchItem *items;
    /*
     Between 1 and 16384.
     */
    size_t item_count;
    /*
     Required readable input pool; borrowed only for the call.
     */
    const struct ZeGraphValuePool *pool;
    /*
     Optional controls; null means defaults, not unbounded work.
     */
    const struct ZeGraphControl *control;
} ZeGraphBatchRequest;

/*
 One completed result column; order and duplicate display names are preserved.
 */
typedef struct ZeGraphColumn {
    /*
     Exact sizeof this version-one descriptor; fixed array stride.
     */
    uint32_t abi_size;
    /*
     Must be zero.
     */
    uint32_t abi_reserved;
    /*
     UTF-8 name bytes in response.pool.bytes.
     */
    struct ZeGraphRange name;
    /*
     Nonzero bitmask Null1 Bool2 I644 F648 String16 Node32 Rel64 List128.
     */
    uint32_t kinds;
    /*
     Must be zero.
     */
    uint32_t reserved;
} ZeGraphColumn;

/*
 One successful structured item outcome; only Committed/Replayed dispositions. Receipts do not authorize generic Cypher retry.
 */
typedef struct ZeGraphReceipt {
    /*
     Exact sizeof this version-one descriptor; fixed array stride.
     */
    uint32_t abi_size;
    /*
     Must be zero.
     */
    uint32_t abi_reserved;
    /*
     Zero-based original batch item index.
     */
    uint32_t item;
    /*
     One ZeGraphEntityKind.
     */
    uint32_t entity_kind;
    /*
     Committed for new change or Replayed for exact retry.
     */
    uint32_t disposition;
    /*
     0/1 deletion outcome; identity denotes affected incarnation.
     */
    uint32_t deleted;
    /*
     Nonzero only for node outcome.
     */
    struct ZeNodeId node;
    /*
     Nonzero only for relationship outcome.
     */
    struct ZeRelId relationship;
    /*
     Installed positive entity/deletion revision.
     */
    uint64_t revision;
    /*
     Original changed generation; mixed batches may retain older replay generations.
     */
    uint64_t generation;
} ZeGraphReceipt;

/*
 One eager invocation report retained through projections/aggregations, even with zero result rows. Component yields distinguish absent membership from numeric zero; cross-scoring is complete for retained candidates.
 */
typedef struct ZeGraphSearchReport {
    /*
     Exact sizeof this version-one descriptor; fixed array stride.
     */
    uint32_t abi_size;
    /*
     Must be zero.
     */
    uint32_t abi_reserved;
    /*
     Unique source-order invocation ID.
     */
    uint32_t call_id;
    /*
     One ZeGraphSearchKind.
     */
    uint32_t kind;
    /*
     Same admitted generation as response.
     */
    uint64_t generation;
    /*
     0/1, preserving omitted preference.
     */
    uint32_t has_requested_tier;
    /*
     One ZeGraphTier when present; zero otherwise.
     */
    uint32_t requested_tier;
    /*
     0/1; absent for lexical-only execution.
     */
    uint32_t has_actual_tier;
    /*
     Exact/Scan/Graph actual vector route; zero if absent.
     */
    uint32_t actual_tier;
    /*
     One ZeGraphScorePrecision; original scores do not imply exact coverage.
     */
    uint32_t precision;
    /*
     One ZeGraphCandidateCoverage.
     */
    uint32_t coverage;
    /*
     One ZeGraphLegState.
     */
    uint32_t vector_leg;
    /*
     One ZeGraphLegState.
     */
    uint32_t lexical_leg;
    /*
     0/1, selected stored document interpretation.
     */
    uint32_t has_document_epoch;
    /*
     0/1, declared/effective query interpretation.
     */
    uint32_t has_query_epoch;
    /*
     0/1, selected lexical interpretation.
     */
    uint32_t has_tokenizer_epoch;
    /*
     0/1: every retained hybrid candidate has all present components evaluated.
     */
    uint32_t cross_score_complete;
    /*
     Selected document epoch; zero if absent.
     */
    uint64_t document_epoch;
    /*
     Selected query epoch; zero if absent.
     */
    uint64_t query_epoch;
    /*
     Selected analyzer epoch; zero if absent.
     */
    uint64_t tokenizer_epoch;
    /*
     Effective query-level vector weight, finite in [0,1]; no per-node renormalization.
     */
    double effective_alpha;
    /*
     Actual scoring-anchor policy version.
     */
    uint32_t normalization_version;
    /*
     Actual alpha/rule policy version.
     */
    uint32_t rules_version;
    /*
     Actual retained candidate union size, deduplicated by full identity/version.
     */
    uint64_t candidate_count;
    /*
     Actual retained candidates whose present modalities were evaluated.
     */
    uint64_t cross_scored_count;
    /*
     Actual fallbacks; zero does not certify exactness.
     */
    uint64_t fallback_count;
    /*
     Invocation-specific rows in response.work; global_work names separate cumulative totals.
     */
    struct ZeGraphRange work;
} ZeGraphSearchReport;

/*
 Fixed optional pool/slot index. Presence exactly 0 or 1; absent index is zero. Index zero is legal when present.
 */
typedef struct ZeGraphOptionalIndex {
    /*
     Exactly zero or one.
     */
    uint32_t present;
    /*
     Named array or logical slot index; never a pointer.
     */
    uint32_t index;
} ZeGraphOptionalIndex;

/*
 Bounded owned error diagnostic, separate from global mutable last-error. Ranges refer to the query source or response pool as named; no borrowed caller strings survive.
 */
typedef struct ZeGraphDiagnostic {
    /*
     Exact sizeof this version-one descriptor; fixed array stride.
     */
    uint32_t abi_size;
    /*
     Must be zero.
     */
    uint32_t abi_reserved;
    /*
     One append-only ZeErrorCode numeric value.
     */
    int32_t code;
    /*
     Must be zero.
     */
    uint32_t reserved;
    /*
     Optional originating plan operator index.
     */
    struct ZeGraphOptionalIndex operator_index;
    /*
     Exactly zero or one; absent source range is zero.
     */
    uint32_t has_source_span;
    /*
     Must be zero.
     */
    uint32_t source_reserved;
    /*
     Byte range in original UTF-8 query source, not character offsets.
     */
    struct ZeGraphRange source_span;
    /*
     Owned diagnostic UTF-8 bytes in response.pool.bytes.
     */
    struct ZeGraphRange message;
} ZeGraphDiagnostic;

/*
 One actual counter; global totals include all invocations and normalization passes. Capacity counters are bytes, never logical lengths.
 */
typedef struct ZeGraphWorkCounter {
    /*
     Exact sizeof this version-one descriptor; fixed array stride.
     */
    uint32_t abi_size;
    /*
     Must be zero.
     */
    uint32_t abi_reserved;
    /*
     One ZeGraphWorkKind.
     */
    uint32_t kind;
    /*
     Must be zero.
     */
    uint32_t reserved;
    /*
     Actual measured count; never requested or estimated work.
     */
    uint64_t value;
} ZeGraphWorkCounter;

/*
 Completed root descriptor. A valid output is emptied before request/handle validation; every nested pointer remains immutable until matching future response_free, including after store close. ABI arena <=4 MiB; registry/control capacity separately charged. This declaration does not implement ownership or exports.
 */
typedef struct ZeGraphResponse {
    /*
     Exact sizeof this version-one descriptor; fixed array stride.
     */
    uint32_t abi_size;
    /*
     Must be zero.
     */
    uint32_t abi_reserved;
    /*
     Opaque registry generation; zero for an empty unowned response. ZE-68 supplies registration/free.
     */
    uint64_t owner_token;
    /*
     One ZeGraphDisposition; meaningful even on nonzero status.
     */
    uint32_t disposition;
    /*
     0/1; failure before admission leaves absent.
     */
    uint32_t has_admitted_generation;
    /*
     Single admitted view generation; zero if absent.
     */
    uint64_t admitted_generation;
    /*
     0/1; only known Committed outcome may expose a new generation.
     */
    uint32_t has_changed_generation;
    /*
     Must be zero.
     */
    uint32_t reserved;
    /*
     Known changed generation; zero if absent.
     */
    uint64_t changed_generation;
    /*
     Complete row count <=65536; rows preserve bag multiplicity.
     */
    size_t row_count;
    /*
     Immutable result columns; null only at zero count.
     */
    const struct ZeGraphColumn *columns;
    /*
     Columns per row, <=256.
     */
    size_t column_count;
    /*
     Row-major indices into pool.values; explicit Null cells represent missing values.
     */
    const uint32_t *cells;
    /*
     Checked row_count * column_count; no partially populated rows.
     */
    size_t cell_count;
    /*
     Root-owned immutable value/name/entity payload pools, independent of caller/store lifetimes.
     */
    struct ZeGraphValuePool pool;
    /*
     Per-item outcomes; no guessed IDs on Indeterminate. Null only at zero count.
     */
    const struct ZeGraphReceipt *receipts;
    /*
     Initialized elements in receipts.
     */
    size_t receipt_count;
    /*
     Every executed eager search report in source order. Null only at zero count.
     */
    const struct ZeGraphSearchReport *reports;
    /*
     Initialized elements in reports.
     */
    size_t report_count;
    /*
     Bounded owned diagnostics, including on errors. Null only at zero count.
     */
    const struct ZeGraphDiagnostic *diagnostics;
    /*
     Initialized elements in diagnostics.
     */
    size_t diagnostic_count;
    /*
     Actual counter rows; global and per-call ranges distinct. Null only at zero count.
     */
    const struct ZeGraphWorkCounter *work;
    /*
     Initialized elements in work.
     */
    size_t work_count;
    /*
     Whole-request cumulative counters in work; includes all preparation and searches.
     */
    struct ZeGraphRange global_work;
} ZeGraphResponse;

/*
 Named nonentity parameter binding; no extra/duplicate/missing names.
 */
typedef struct ZeGraphParameterValue {
    /*
     Exact sizeof this version-one descriptor; fixed array stride.
     */
    uint32_t abi_size;
    /*
     Must be zero.
     */
    uint32_t abi_reserved;
    /*
     Name bytes in the request parameter pool.
     */
    struct ZeGraphRange name;
    /*
     Value index in that parameter pool.
     */
    uint32_t value;
    /*
     Must be zero.
     */
    uint32_t reserved;
} ZeGraphParameterValue;

/*
 Explicit tightened work allowance, including zero. Unknown/duplicate categories and widening hard limits reject. Omitted categories retain current hard defaults.
 */
typedef struct ZeGraphWorkLimit {
    /*
     Exact sizeof this version-one descriptor; fixed array stride.
     */
    uint32_t abi_size;
    /*
     Must be zero.
     */
    uint32_t abi_reserved;
    /*
     One runtime ZeGraphWorkKind 0..21; PeakOwnedBytes is not a work allowance.
     */
    uint32_t kind;
    /*
     Must be zero.
     */
    uint32_t reserved;
    /*
     Inclusive maximum actual cumulative units; zero is a real allowance.
     */
    uint64_t limit;
} ZeGraphWorkLimit;

/*
 Optional query-local tightening, inside shared store accounting; these declarations never prove actual reservations or allocator ownership.
 */
typedef struct ZeGraphQueryLimits {
    /*
     Exact sizeof this version-one descriptor; fixed array stride.
     */
    uint32_t abi_size;
    /*
     Must be zero.
     */
    uint32_t abi_reserved;
    /*
     0/1 explicit memory ceiling presence; zero ceiling is permitted and execution may fail to reserve even its context.
     */
    uint32_t has_query_bytes;
    /*
     Must be zero.
     */
    uint32_t reserved;
    /*
     When present, actual retained-capacity ceiling <=24 MiB; zero if absent.
     */
    uint64_t query_bytes;
    /*
     At most one row per category; null only at zero count.
     */
    const struct ZeGraphWorkLimit *work;
    /*
     Between zero and 22; bounded validation before execution.
     */
    size_t work_count;
} ZeGraphQueryLimits;

/*
 Query interpretation declaration. Query tower/alignment metadata does not alter the document interpretation stored at open.
 */
typedef struct ZeGraphQueryOptions {
    /*
     Exact sizeof this version-one descriptor; fixed array stride.
     */
    uint32_t abi_size;
    /*
     Must be zero.
     */
    uint32_t abi_reserved;
    /*
     Optional declared query-tower identity; compatibility checked with document tower.
     */
    const ZeEmbeddingTower *query_tower;
    /*
     Optional opaque alignment digest bytes; empty means absent.
     */
    struct ZeGraphBytes alignment_digest;
    /*
     Optional typed memory/work tightening; null uses existing hard defaults.
     */
    const struct ZeGraphQueryLimits *limits;
} ZeGraphQueryOptions;

/*
 Full caller-tightened outer compiler limits. Null request pointer selects defaults; when present every field is explicit, including zero. All compiler capacities still count inside the same query budget.
 */
typedef struct ZeGraphCompileLimits {
    /*
     Exact sizeof this version-one descriptor; fixed array stride.
     */
    uint32_t abi_size;
    /*
     Must be zero.
     */
    uint32_t abi_reserved;
    /*
     UTF-8 source bytes <=65536.
     */
    uint32_t text_bytes;
    /*
     Lexical tokens <=8192.
     */
    uint32_t tokens;
    /*
     AST nodes <=4096.
     */
    uint32_t ast_nodes;
    /*
     Nesting depth <=64.
     */
    uint32_t depth;
    /*
     Named parameters <=256.
     */
    uint32_t parameters;
    /*
     Projected columns per scope <=256.
     */
    uint32_t columns;
    /*
     Query list nesting <=16.
     */
    uint32_t list_depth;
    /*
     Finite path upper bound <=16.
     */
    uint32_t path_hops;
} ZeGraphCompileLimits;

/*
 Outer compiler request; only typed plans enter core. Invalid/unsupported syntax returns owned bounded diagnostics and no effects.
 */
typedef struct ZeGraphCypherRequest {
    /*
     Exact sizeof this version-one descriptor; fixed array stride.
     */
    uint32_t abi_size;
    /*
     Must be zero.
     */
    uint32_t abi_reserved;
    /*
     One UTF-8 query text, at most 64 KiB; no binary plan encoding.
     */
    struct ZeGraphBytes query;
    /*
     Named typed bindings; null only at zero count.
     */
    const struct ZeGraphParameterValue *parameters;
    /*
     Number of parameter bindings.
     */
    size_t parameter_count;
    /*
     Required if bindings exist; otherwise optional.
     */
    const struct ZeGraphValuePool *parameter_pool;
    /*
     Optional query interpretation.
     */
    const struct ZeGraphQueryOptions *options;
    /*
     Optional controls covering parsing through completion.
     */
    const struct ZeGraphControl *control;
    /*
     Optional complete compiler limit tightening; null selects existing defaults.
     */
    const struct ZeGraphCompileLimits *compile_limits;
} ZeGraphCypherRequest;

/*
 Fixed expression descriptor. Fields not named by kind are zero. Aggregate uses operation, has_operand, left and distinct; unary uses operation/left; binary uses operation/left/right.
 */
typedef struct ZeGraphExpression {
    /*
     Exact sizeof this version-one descriptor; fixed array stride.
     */
    uint32_t abi_size;
    /*
     Must be zero.
     */
    uint32_t abi_reserved;
    /*
     One ZeGraphExpressionKind.
     */
    uint32_t kind;
    /*
     Corresponding unary/binary/aggregate discriminant; zero otherwise.
     */
    uint32_t operation;
    /*
     First expression operand; used by unary/binary/property/label/aggregate.
     */
    uint32_t left;
    /*
     Second expression operand for Binary; zero otherwise.
     */
    uint32_t right;
    /*
     Aggregate only: 0 for count(*), 1 for an operand.
     */
    uint32_t has_operand;
    /*
     Aggregate only: 0 or 1.
     */
    uint32_t distinct;
    /*
     Literal: pool value index. Slot: logical slot ID. Parameter: declaration index.
     */
    uint32_t value;
    /*
     Must be zero.
     */
    uint32_t reserved;
    /*
     Property/HasLabel name bytes in plan pool.
     */
    struct ZeGraphRange name;
    /*
     List only: range in plan.expression_children.
     */
    struct ZeGraphRange children;
} ZeGraphExpression;

/*
 One output binding; expression is evaluated in the input scope.
 */
typedef struct ZeGraphProjection {
    /*
     Exact sizeof this version-one descriptor; fixed array stride.
     */
    uint32_t abi_size;
    /*
     Must be zero.
     */
    uint32_t abi_reserved;
    /*
     Logical u32 slot ID, not a row offset; per-scope width at most 256.
     */
    uint32_t slot;
    /*
     Expression index.
     */
    uint32_t expression;
} ZeGraphProjection;

/*
 One ordered comparison key; later keys break ties.
 */
typedef struct ZeGraphSortKey {
    /*
     Exact sizeof this version-one descriptor; fixed array stride.
     */
    uint32_t abi_size;
    /*
     Must be zero.
     */
    uint32_t abi_reserved;
    /*
     Expression in current scope.
     */
    uint32_t expression;
    /*
     Exactly zero or one.
     */
    uint32_t descending;
} ZeGraphSortKey;

/*
 Parameter declaration; nested entity values are forbidden too.
 */
typedef struct ZeGraphParameter {
    /*
     Exact sizeof this version-one descriptor; fixed array stride.
     */
    uint32_t abi_size;
    /*
     Must be zero.
     */
    uint32_t abi_reserved;
    /*
     Exact UTF-8 name bytes in plan pool.
     */
    struct ZeGraphRange name;
    /*
     Nonzero mask: Null1 Bool2 I644 F648 String16 List128; bits32/64 forbidden.
     */
    uint32_t kinds;
    /*
     Must be zero.
     */
    uint32_t reserved;
} ZeGraphParameter;

/*
 Typed vector/lexical/fusion options. Irrelevant options reject; defaults and absent preferences retain existing Store semantics. Window is a separate evaluated expression in ZeGraphSearch.
 */
typedef struct ZeGraphSearchOptions {
    /*
     Exact sizeof this version-one descriptor; fixed array stride.
     */
    uint32_t abi_size;
    /*
     Must be zero.
     */
    uint32_t abi_reserved;
    /*
     0 SIFT-class or 1 angular; used by structured Graph/Auto routing.
     */
    uint32_t graph_profile;
    /*
     0 adaptive or explicit positive width within admitted work limits.
     */
    uint32_t graph_ef;
    /*
     Deterministic query preparation seed.
     */
    uint64_t graph_seed;
    /*
     Only bit0 last-as-prefix is accepted; zero otherwise.
     */
    uint32_t lexical_flags;
    /*
     0 default, 1 original-f32 rescore; never implies exhaustive coverage.
     */
    uint32_t rescore;
    /*
     Exactly 0 or 1; absent alpha bits are positive zero.
     */
    uint32_t has_alpha;
    /*
     Exactly 0 or 1; query-level alpha rules, ignored by explicit alpha.
     */
    uint32_t rules_enabled;
    /*
     Explicit finite convex weight in [0,1].
     */
    double alpha;
    /*
     Exactly 0 or 1; absent max_rounds is zero.
     */
    uint32_t has_max_rounds;
    /*
     Must be zero.
     */
    uint32_t reserved;
    /*
     Explicit widening limit; zero means full-list strategy within shared caps.
     */
    uint64_t max_rounds;
} ZeGraphSearchOptions;

/*
 Eager uncorrelated invocation. Runtime validates evaluated k/window before work; each syntactic call remains required even under LIMIT 0. Eligibility is a query-local typed set slot, never external IDs.
 */
typedef struct ZeGraphSearch {
    /*
     Exact sizeof this version-one descriptor; fixed array stride.
     */
    uint32_t abi_size;
    /*
     Must be zero.
     */
    uint32_t abi_reserved;
    /*
     One ZeGraphSearchKind.
     */
    uint32_t kind;
    /*
     Unique source-order ID; eager_searches must retain every call in order.
     */
    uint32_t call_id;
    /*
     Vector expression for Vector/Hybrid; absent for Text.
     */
    struct ZeGraphOptionalIndex vector;
    /*
     Text expression for Text/Hybrid; absent for Vector.
     */
    struct ZeGraphOptionalIndex text;
    /*
     Expression yielding positive I64 <=4096.
     */
    uint32_t k;
    /*
     Exactly 0 or 1; absent tier field must be zero.
     */
    uint32_t has_tier;
    /*
     One ZeGraphTier; text-only tier must be absent.
     */
    uint32_t tier;
    /*
     Must be zero.
     */
    uint32_t reserved;
    /*
     Logical eligible-set slot, produced by EligibleSet operator; absence means AllIndexed.
     */
    struct ZeGraphOptionalIndex eligible_set;
    /*
     Optional expression yielding nonnegative I64 <=65536; zero is an explicit tightened allowance, not default.
     */
    struct ZeGraphOptionalIndex window;
    /*
     Node output slot.
     */
    uint32_t node_slot;
    /*
     Distance for Vector, score for Text/Hybrid.
     */
    uint32_t score_slot;
    /*
     Hybrid only: optional nullable original component distance output slot.
     */
    struct ZeGraphOptionalIndex vector_distance_slot;
    /*
     Hybrid only: optional nullable BM25 output slot.
     */
    struct ZeGraphOptionalIndex lexical_score_slot;
    /*
     Optional typed options; null means defaults.
     */
    const struct ZeGraphSearchOptions *options;
} ZeGraphSearch;

/*
 Fixed typed operator descriptor. Only kind-documented fields are active; all others zero. Inputs and range fields refer to named plan arrays. Search may have one eligibility dependency in inputs when eligible_set is present; no row correlation.
 */
typedef struct ZeGraphOperator {
    /*
     Exact sizeof this version-one descriptor; fixed array stride.
     */
    uint32_t abi_size;
    /*
     Must be zero.
     */
    uint32_t abi_reserved;
    /*
     One ZeGraphOperatorKind.
     */
    uint32_t kind;
    /*
     LookupKey only: one ZeGraphEntityKind.
     */
    uint32_t entity_kind;
    /*
     Ordered operator indices in plan.inputs; exact arity depends on kind.
     */
    struct ZeGraphRange inputs;
    /*
     Filter required, Join/OptionalApply optional; expression index.
     */
    struct ZeGraphOptionalIndex predicate;
    /*
     Expand/BoundedExpand/EligibleSet input node slot.
     */
    uint32_t source_slot;
    /*
     Scan/lookup/expand node or LookupKey entity output.
     */
    uint32_t node_slot;
    /*
     LookupRelationship/Expand entity or BoundedExpand list output.
     */
    uint32_t relationship_slot;
    /*
     EligibleSet output logical set ID; not a row value or external handle.
     */
    uint32_t set_slot;
    /*
     Expand/BoundedExpand: 0 outgoing, 1 incoming, 2 either.
     */
    uint32_t direction;
    /*
     Expand/BoundedExpand MATCH uniqueness scope; origins preserved across joins.
     */
    uint32_t pattern;
    /*
     Scan optional label or LookupKey namespace bytes in pool.
     */
    struct ZeGraphRange name;
    /*
     Scan only: 0/1; LookupKey name is required and has_name=1.
     */
    uint32_t has_name;
    /*
     LookupKey only: string key expression index.
     */
    uint32_t key_expression;
    /*
     LookupNode only; zero otherwise.
     */
    struct ZeNodeId node_id;
    /*
     LookupRelationship only; zero otherwise.
     */
    struct ZeRelId relationship_id;
    /*
     OR-ed type-name ranges in pool.names; zero count means unconstrained.
     */
    struct ZeGraphRange relationship_types;
    /*
     BoundedExpand inclusive lower bound, 0..16.
     */
    uint32_t path_min;
    /*
     BoundedExpand inclusive upper bound, >=min and <=16.
     */
    uint32_t path_max;
    /*
     BoundedExpand per-edge expression, evaluated using edge_slot plus input scope.
     */
    struct ZeGraphOptionalIndex edge_predicate;
    /*
     Temporary relationship slot visible only in edge_predicate; zero if absent.
     */
    uint32_t edge_slot;
    /*
     Search only: plan.searches index.
     */
    uint32_t search;
    /*
     Project/With output or Aggregate group keys in plan.projections.
     */
    struct ZeGraphRange projections;
    /*
     Aggregate output projections in plan.projections.
     */
    struct ZeGraphRange aggregates;
    /*
     Sort only: plan.sort_keys range.
     */
    struct ZeGraphRange sort_keys;
    /*
     Mutate only: plan.mutations range.
     */
    struct ZeGraphRange mutations;
    /*
     OffsetLimit skipped rows.
     */
    uint64_t offset;
    /*
     OffsetLimit maximum rows if has_limit=1; zero otherwise.
     */
    uint64_t limit;
    /*
     OffsetLimit only: exactly 0 or 1.
     */
    uint32_t has_limit;
    /*
     Must be zero.
     */
    uint32_t reserved;
} ZeGraphOperator;

/*
 One query mutation. Inactive fields zero. No retry/upsert implication; images and identity allocation belong to staging.
 */
typedef struct ZeGraphMutation {
    /*
     Exact sizeof this version-one descriptor; fixed array stride.
     */
    uint32_t abi_size;
    /*
     Must be zero.
     */
    uint32_t abi_reserved;
    /*
     One ZeGraphMutationKind.
     */
    uint32_t kind;
    /*
     CreateNode/CreateRelationship destination slot.
     */
    uint32_t output;
    /*
     Entity expression for noncreate items.
     */
    uint32_t entity;
    /*
     SetProperty value expression.
     */
    uint32_t value;
    /*
     CreateRelationship source node expression.
     */
    uint32_t source;
    /*
     CreateRelationship target node expression.
     */
    uint32_t target;
    /*
     SetLabel only: exactly 0 or 1.
     */
    uint32_t present;
    /*
     Delete only: exactly 0 or 1.
     */
    uint32_t detach;
    /*
     Property/label/type name in plan pool.bytes.
     */
    struct ZeGraphRange name;
    /*
     CreateNode only: label ranges in plan pool.names.
     */
    struct ZeGraphRange labels;
} ZeGraphMutation;

/*
 Fixed-stride typed plan arenas. ABI shape validation is not DAG/type/scope/liveness admission. Runtime must converge on core validation and reject unsupported descriptors, never silently drop fields.
 */
typedef struct ZeGraphPlan {
    /*
     Exact sizeof this version-one descriptor; fixed array stride.
     */
    uint32_t abi_size;
    /*
     Must be zero.
     */
    uint32_t abi_reserved;
    /*
     Row-producing operator index.
     */
    uint32_t root;
    /*
     Must be zero.
     */
    uint32_t reserved;
    /*
     At most 4096 DAG nodes. Null only at zero count.
     */
    const struct ZeGraphOperator *operators;
    /*
     Accessible elements of operators.
     */
    size_t operator_count;
    /*
     At most 4096 expressions; maximum depth 64. Null only at zero count.
     */
    const struct ZeGraphExpression *expressions;
    /*
     Accessible elements of expressions.
     */
    size_t expression_count;
    /*
     Ordered operator dependency indices. Null only at zero count.
     */
    const uint32_t *inputs;
    /*
     Accessible elements of inputs.
     */
    size_t input_count;
    /*
     Ordered list expression indices. Null only at zero count.
     */
    const uint32_t *expression_children;
    /*
     Accessible elements of expression_children.
     */
    size_t expression_child_count;
    /*
     Output/group bindings. Null only at zero count.
     */
    const struct ZeGraphProjection *projections;
    /*
     Accessible elements of projections.
     */
    size_t projection_count;
    /*
     Ordered comparison keys. Null only at zero count.
     */
    const struct ZeGraphSortKey *sort_keys;
    /*
     Accessible elements of sort_keys.
     */
    size_t sort_key_count;
    /*
     Progressive overlay mutation descriptors. Null only at zero count.
     */
    const struct ZeGraphMutation *mutations;
    /*
     Accessible elements of mutations.
     */
    size_t mutation_count;
    /*
     Unique nonentity parameter declarations. Null only at zero count.
     */
    const struct ZeGraphParameter *parameters;
    /*
     Accessible elements of parameters.
     */
    size_t parameter_count;
    /*
     At most eight eager source descriptors. Null only at zero count.
     */
    const struct ZeGraphSearch *searches;
    /*
     Accessible elements of searches.
     */
    size_t search_count;
    /*
     Search operator indices in validated source order, regardless of row reachability. Null only at zero count.
     */
    const uint32_t *eager_searches;
    /*
     Accessible elements of eager_searches.
     */
    size_t eager_search_count;
    /*
     Required literals and name pool; no entity literals.
     */
    const struct ZeGraphValuePool *pool;
} ZeGraphPlan;

/*
 Synchronous structured query; caller buffers borrowed only until return. No result can retain a view or caller pointer.
 */
typedef struct ZeGraphQueryRequest {
    /*
     Exact sizeof this version-one descriptor; fixed array stride.
     */
    uint32_t abi_size;
    /*
     Must be zero.
     */
    uint32_t abi_reserved;
    /*
     Required typed plan.
     */
    const struct ZeGraphPlan *plan;
    /*
     Named bindings; null only at count zero.
     */
    const struct ZeGraphParameterValue *parameters;
    /*
     Number of bindings.
     */
    size_t parameter_count;
    /*
     Required when any binding exists; otherwise optional.
     */
    const struct ZeGraphValuePool *parameter_pool;
    /*
     Optional query options.
     */
    const struct ZeGraphQueryOptions *options;
    /*
     Optional cancellation/deadline controls.
     */
    const struct ZeGraphControl *control;
} ZeGraphQueryRequest;

/*
 Single-admission typed entity read. Returns one column, one row per requested ID, explicit Null for missing IDs; no second snapshot or automatic query follow-up.
 */
typedef struct ZeGraphGetNodesRequest {
    /*
     Exact sizeof this version-one descriptor; fixed array stride.
     */
    uint32_t abi_size;
    /*
     Must be zero.
     */
    uint32_t abi_reserved;
    /*
     Nonzero store-local IDs; null only at zero count.
     */
    const struct ZeNodeId *ids;
    /*
     Input count; completed result bounds apply, order and duplicates preserved.
     */
    size_t id_count;
    /*
     0/1 explicit source text selection; absent differs from selected empty.
     */
    uint32_t include_text;
    /*
     0/1 explicit original vector selection; dimension validated.
     */
    uint32_t include_vector;
    /*
     Optional cancellation/deadline controls.
     */
    const struct ZeGraphControl *control;
    /*
     Optional typed memory/work tightening; null uses existing hard defaults.
     */
    const struct ZeGraphQueryLimits *limits;
} ZeGraphGetNodesRequest;

/*
 Single-admission typed entity read. Returns one column, one row per requested ID, explicit Null for missing IDs; no second snapshot or automatic query follow-up.
 */
typedef struct ZeGraphGetRelsRequest {
    /*
     Exact sizeof this version-one descriptor; fixed array stride.
     */
    uint32_t abi_size;
    /*
     Must be zero.
     */
    uint32_t abi_reserved;
    /*
     Nonzero store-local IDs; null only at zero count.
     */
    const struct ZeRelId *ids;
    /*
     Input count; completed result bounds apply, order and duplicates preserved.
     */
    size_t id_count;
    /*
     Optional cancellation/deadline controls.
     */
    const struct ZeGraphControl *control;
    /*
     Optional typed memory/work tightening; null uses existing hard defaults.
     */
    const struct ZeGraphQueryLimits *limits;
} ZeGraphGetRelsRequest;

#ifdef __cplusplus
extern "C" {
#endif // __cplusplus

/*
 Opens (`mode` 1 read-write, 2 read-only) or creates (`mode` 0) one native
 graph store; a legacy store directory is refused with
 `ZE_ERR_STORE_KIND`. `max_resident_bytes` must be in 1..=256 MiB,
 `tokenizer_profile` must be 0 and `control` must be null.
 `document_tower` is null for a store without vectors; otherwise node
 vectors are validated against it and it must match the persisted tower.
 */
ze_error_code ze_graph_open(const struct ZeGraphOpenRequest *request,
                            struct ZeGraphHandle *out_handle);

/*
 Closes a graph store and releases its handle; outstanding responses stay
 valid until freed. Closing a stale or closed handle is `ZE_ERR_CLOSED`.
 */
ze_error_code ze_graph_close(struct ZeGraphHandle handle);

/*
 Applies one atomic structured batch: every node (document) and
 relationship item commits durably together, or none does. On success
 `out_response` holds one receipt per item in item order, the disposition
 and the admitted and changed generations; free it with
 `ze_graph_response_free`. An exact keyed retry replays.
 */
ze_error_code ze_graph_apply(struct ZeGraphHandle handle,
                             const struct ZeGraphBatchRequest *request,
                             struct ZeGraphResponse *out_response);

/*
 Releases one response and resets it to the empty descriptor. An empty
 response, including one an error left behind, is accepted; freeing again
 is a no-op. A forged or altered descriptor is `ZE_ERR_INVALID_ARGUMENT`.
 */
ze_error_code ze_graph_response_free(struct ZeGraphResponse *response);

/*
 Compiles and executes one Cypher statement with scalar parameters and a
 default maximum of 1,024 returned rows. Options must be null.
 */
ze_error_code ze_graph_cypher(struct ZeGraphHandle handle,
                              const struct ZeGraphCypherRequest *request,
                              struct ZeGraphResponse *out_response);

/*
 Executes Cypher using the frozen request layout and a caller-selected
 returned-row cap: 0 selects 1,024; 1..=65,536 is accepted. Exceeding the
 cap fails, never truncates. Other work and memory budgets still apply.
 */
ze_error_code ze_graph_cypher_with_row_limit(struct ZeGraphHandle handle,
                                             const struct ZeGraphCypherRequest *request,
                                             uint32_t result_row_limit,
                                             struct ZeGraphResponse *out_response);

#ifdef __cplusplus
}  // extern "C"
#endif  // __cplusplus

#endif  /* ZEPPELIN_GRAPH_CONTRACTS_H */
