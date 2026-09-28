//! Opt-in graph ABI data contracts. Runtime marshalling and owned result
//! registration are separate capabilities and are not implemented by these types.

/// Store-local nonzero node identity. No relationship or document conversion.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
#[repr(C)]
pub struct ZeNodeId {
    /// Most significant 64 bits.
    pub high: u64,
    /// Least significant 64 bits.
    pub low: u64,
}

/// Store-local nonzero relationship identity, distinct from a node identity.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
#[repr(C)]
pub struct ZeRelId {
    /// Most significant 64 bits.
    pub high: u64,
    /// Least significant 64 bits.
    pub low: u64,
}

/// Opaque graph-only generation-tagged handle. Zero is never a live handle.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
#[repr(C)]
pub struct ZeGraphHandle {
    /// The handle registry owns interpretation; this is not a store pointer.
    pub token: u64,
}

/// Fixed pool span. `start` and `count` use the named target array's elements.
/// Checked addition and full containment are required before dereferencing.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
#[repr(C)]
pub struct ZeGraphRange {
    /// Zero-based first element; zero for an unused range.
    pub start: u32,
    /// Number of elements; an empty range remains distinct from absent data.
    pub count: u32,
}

/// Value discriminants. Input fields store u32, not this Rust enum, so unknown
/// C values can be rejected safely before interpretation.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[repr(u32)]
pub enum ZeGraphValueTag {
    /// Explicit null, never an empty string/list or sentinel entity.
    ZeGraphValueNull = 0,
    /// Boolean field, exactly zero or one.
    ZeGraphValueBool = 1,
    /// Exact signed integer field.
    ZeGraphValueI64 = 2,
    /// Exact IEEE binary64 field; nonfinite scalar values are permitted.
    ZeGraphValueF64 = 3,
    /// UTF-8 byte range in the string arena; embedded NUL is allowed.
    ZeGraphValueString = 4,
    /// Index into copied node descriptors; never a forgeable engine reference.
    ZeGraphValueNode = 5,
    /// Index into copied relationship descriptors.
    ZeGraphValueRelationship = 6,
    /// Ordered range of value indices in the child-index arena.
    ZeGraphValueList = 7,
}

/// Exact list representation tag, independent of query equality.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[repr(u32)]
pub enum ZeGraphListKind {
    /// General query list; mixed/nested/null elements are permitted within bounds.
    ZeGraphListQuery = 0,
    /// Homogeneous booleans, including a typed empty list.
    ZeGraphListBool = 1,
    /// Homogeneous signed integers, including a typed empty list.
    ZeGraphListI64 = 2,
    /// Homogeneous binary64 values, including a typed empty list.
    ZeGraphListF64 = 3,
    /// Homogeneous UTF-8 strings, including a typed empty list.
    ZeGraphListString = 4,
    /// Stored untyped empty-list sentinel; count must be zero.
    ZeGraphListEmpty = 5,
}

/// Version-one flat value descriptor. `abi_size` must equal sizeof this type.
/// Reserved and inactive fields are zero (including the bits of inactive F64).
/// Only List uses list_kind; String/List use range; Node/Relationship use
/// entity_index; each scalar uses its matching named field. Array bounds,
/// UTF-8, graph ownership and descendant geometry require the later marshaller.
#[derive(Clone, Copy, Debug)]
#[repr(C)]
pub struct ZeGraphValue {
    /// Exact version-one descriptor size; fixed array stride.
    pub abi_size: u32,
    /// Must be zero.
    pub abi_reserved: u32,
    /// One ZeGraphValueTag discriminant; unknown tags reject.
    pub tag: u32,
    /// One ZeGraphListKind for List; zero for all other tags.
    pub list_kind: u32,
    /// Zero or one for Bool; zero otherwise.
    pub boolean: u32,
    /// Copied Node/Relationship array index; zero for other tags.
    pub entity_index: u32,
    /// Exact I64 value; zero otherwise.
    pub integer: i64,
    /// Exact F64 bits; positive-zero bits otherwise.
    pub floating: f64,
    /// String bytes or List child-index elements; zero range otherwise.
    pub range: ZeGraphRange,
}

/// Entity domain, independent of value and operation tags.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[repr(u32)]
pub enum ZeGraphEntityKind {
    /// Node domain.
    ZeGraphEntityNode = 0,
    /// Relationship domain.
    ZeGraphEntityRelationship = 1,
}

/// Endpoint reference for this batch only.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[repr(u32)]
pub enum ZeGraphEndpointKind {
    /// Unused descriptor; all payload fields zero.
    ZeGraphEndpointUnused = 0,
    /// Existing nonzero NodeId.
    ZeGraphEndpointNode = 1,
    /// Zero-based batch item creating or replaying a node.
    ZeGraphEndpointLocal = 2,
}

/// Exact structured key lifecycle operation.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[repr(u32)]
pub enum ZeGraphBatchOperation {
    /// Expect absence and install full image; exact retry is replayable.
    ZeGraphBatchCreate = 0,
    /// Expect matching incarnation and replace full image.
    ZeGraphBatchPut = 1,
    /// Expect matching incarnation and install deletion fence.
    ZeGraphBatchDelete = 2,
    /// Expect deletion revision; allocate a new incarnation.
    ZeGraphBatchRecreate = 3,
}

/// Fixed caller-owned byte span; length is bytes, not NUL termination.
#[derive(Clone, Copy, Debug)]
#[repr(C)]
pub struct ZeGraphBytes {
    /// Accessible bytes for the synchronous call; null only at count zero.
    pub data: *const u8,
    /// Accessible byte count; UTF-8/domain rules depend on the named field.
    pub count: usize,
}

/// Cooperative request controls; existing cancellation token registry is reused.
#[derive(Clone, Copy, Debug)]
#[repr(C)]
pub struct ZeGraphControl {
    /// Exact sizeof this version-one descriptor; fixed array stride.
    pub abi_size: u32,
    /// Must be zero.
    pub abi_reserved: u32,
    /// Zero means no explicit token; nonzero must name a live cancellation token.
    pub cancel_token: u64,
    /// Relative monotonic deadline from call entry; zero means absent.
    pub deadline_ns: u64,
}

/// Property entry. Input names are unique; null is not a stored value.
#[derive(Clone, Copy, Debug)]
#[repr(C)]
pub struct ZeGraphProperty {
    /// Exact sizeof this version-one descriptor; fixed array stride.
    pub abi_size: u32,
    /// Must be zero.
    pub abi_reserved: u32,
    /// UTF-8 name bytes in the value pool.
    pub name: ZeGraphRange,
    /// Index into pool.values.
    pub value: u32,
    /// Must be zero.
    pub reserved: u32,
}

/// Copied node record or full node input image. Input image identity and metadata are zero; key/revision come from the batch item. Output payload fields require explicit selection.
#[derive(Clone, Copy, Debug)]
#[repr(C)]
pub struct ZeGraphNode {
    /// Exact sizeof this version-one descriptor; fixed array stride.
    pub abi_size: u32,
    /// Must be zero.
    pub abi_reserved: u32,
    /// Full output identity; zero in input images.
    pub id: ZeNodeId,
    /// Exactly 0 or 1; absence requires zero namespace/key ranges.
    pub has_key: u32,
    /// Exactly 0 or 1, independent of text length; absent range is zero.
    pub has_text: u32,
    /// Exactly 0 or 1, independent of vector length; absent range is zero.
    pub has_vector: u32,
    /// Must be zero.
    pub reserved: u32,
    /// Application-key namespace bytes; absent for unkeyed entities.
    pub namespace_name: ZeGraphRange,
    /// Application-key bytes, not an implicit user property.
    pub key: ZeGraphRange,
    /// Current positive revision on outputs; zero in fresh input images.
    pub revision: u64,
    /// Original last-change generation on outputs; zero in input images.
    pub last_change_generation: u64,
    /// Range of pool.properties entries.
    pub properties: ZeGraphRange,
    /// Optional source text bytes in pool.bytes.
    pub text: ZeGraphRange,
    /// Optional original finite f32 coordinates in pool.vectors.
    pub vector: ZeGraphRange,
    /// Range of pool.names UTF-8 label ranges; interpreted as a set.
    pub labels: ZeGraphRange,
}

/// Copied relationship or full input image. Input IDs/endpoints/key/revision metadata are zero; batch item endpoints and precondition own those fields. Type is always present.
#[derive(Clone, Copy, Debug)]
#[repr(C)]
pub struct ZeGraphRelationship {
    /// Exact sizeof this version-one descriptor; fixed array stride.
    pub abi_size: u32,
    /// Must be zero.
    pub abi_reserved: u32,
    /// Full output identity; zero in input images.
    pub id: ZeRelId,
    /// Fixed source endpoint on outputs; zero in input images.
    pub source: ZeNodeId,
    /// Fixed target endpoint on outputs; zero in input images.
    pub target: ZeNodeId,
    /// Exactly 0 or 1; absence requires zero namespace/key ranges.
    pub has_key: u32,
    /// Must be zero.
    pub reserved: u32,
    /// Application-key namespace bytes; absent for unkeyed entities.
    pub namespace_name: ZeGraphRange,
    /// Application-key bytes, not an implicit user property.
    pub key: ZeGraphRange,
    /// Current positive revision on outputs; zero in fresh input images.
    pub revision: u64,
    /// Original last-change generation on outputs; zero in input images.
    pub last_change_generation: u64,
    /// Range of pool.properties entries.
    pub properties: ZeGraphRange,
    /// Exactly one byte-exact relationship-type name (empty/NUL permitted) in pool.bytes.
    pub relationship_type: ZeGraphRange,
}

/// Flat borrowed input or immutable owned output pool. Every pointer/count and range must be validated before traversal; ownership is supplied by the later adapter.
#[derive(Clone, Copy, Debug)]
#[repr(C)]
pub struct ZeGraphValuePool {
    /// Exact sizeof this version-one descriptor; fixed array stride.
    pub abi_size: u32,
    /// Must be zero.
    pub abi_reserved: u32,
    /// Flat scalar/list/entity descriptors. Null only at zero count.
    pub values: *const ZeGraphValue,
    /// Accessible elements in values; checked multiplication required.
    pub value_count: usize,
    /// List child indices into values; order and multiplicity preserved. Null only at zero count.
    pub children: *const u32,
    /// Accessible elements in children; checked multiplication required.
    pub child_count: usize,
    /// UTF-8 bytes; strings may contain NUL except where a domain forbids it. Null only at zero count.
    pub bytes: *const u8,
    /// Accessible elements in bytes; checked multiplication required.
    pub byte_count: usize,
    /// Copied nodes or batch input images; never runtime references. Null only at zero count.
    pub nodes: *const ZeGraphNode,
    /// Accessible elements in nodes; checked multiplication required.
    pub node_count: usize,
    /// Copied relationships or batch input images. Null only at zero count.
    pub relationships: *const ZeGraphRelationship,
    /// Accessible elements in relationships; checked multiplication required.
    pub relationship_count: usize,
    /// Property map entries. Null only at zero count.
    pub properties: *const ZeGraphProperty,
    /// Accessible elements in properties; checked multiplication required.
    pub property_count: usize,
    /// Name ranges into bytes; labels/types use their named domain rules. Null only at zero count.
    pub names: *const ZeGraphRange,
    /// Accessible elements in names; checked multiplication required.
    pub name_count: usize,
    /// Original finite vector coordinates; scalar F64 remains unrestricted. Null only at zero count.
    pub vectors: *const f32,
    /// Accessible elements in vectors; checked multiplication required.
    pub vector_count: usize,
}

/// Tagged existing/local node endpoint. Inactive fields zero; kind Unused is only legal in non-relationship or delete items.
#[derive(Clone, Copy, Debug)]
#[repr(C)]
pub struct ZeGraphEndpoint {
    /// Exact sizeof this version-one descriptor; fixed array stride.
    pub abi_size: u32,
    /// Must be zero.
    pub abi_reserved: u32,
    /// One ZeGraphEndpointKind value.
    pub kind: u32,
    /// Batch item index for Local; zero otherwise.
    pub local_item: u32,
    /// Nonzero NodeId for Node; zero otherwise.
    pub node: ZeNodeId,
}

/// One keyed full-record operation, at most 16384 per atomic batch. No caller-selected fresh identity. Input image optional only for Delete; relationship endpoints are required for nondelete relationship items.
#[derive(Clone, Copy, Debug)]
#[repr(C)]
pub struct ZeGraphBatchItem {
    /// Exact sizeof this version-one descriptor; fixed array stride.
    pub abi_size: u32,
    /// Must be zero.
    pub abi_reserved: u32,
    /// One ZeGraphEntityKind.
    pub entity_kind: u32,
    /// One ZeGraphBatchOperation.
    pub operation: u32,
    /// Byte-exact application namespace (empty/NUL permitted) in request pool.bytes.
    pub namespace_name: ZeGraphRange,
    /// Byte-exact application key (empty/NUL permitted) in request pool.bytes.
    pub key: ZeGraphRange,
    /// Requested positive revision; checked against existing state.
    pub revision: u64,
    /// Only node Put/Delete: expected nonzero incarnation; zero otherwise.
    pub expected_node: ZeNodeId,
    /// Only relationship Put/Delete: expected nonzero incarnation; zero otherwise.
    pub expected_relationship: ZeRelId,
    /// Positive only for Recreate; zero otherwise.
    pub expected_deletion_revision: u64,
    /// Delete only: 0 Restrict, 1 Detach (nodes only); zero otherwise.
    pub delete_mode: u32,
    /// Exactly 1 for Create/Put/Recreate, 0 for Delete.
    pub has_image: u32,
    /// Index into pool.nodes or pool.relationships by entity_kind; zero if absent.
    pub image: u32,
    /// Must be zero.
    pub reserved: u32,
    /// Relationship source input; unused for nodes and deletes.
    pub source: ZeGraphEndpoint,
    /// Relationship target input; unused for nodes and deletes.
    pub target: ZeGraphEndpoint,
}

/// One synchronous atomic structured batch. Total canonical input including framing is at most 8 MiB; runtime/staging validates ownership and exact replay.
#[derive(Clone, Copy, Debug)]
#[repr(C)]
pub struct ZeGraphBatchRequest {
    /// Exact sizeof this version-one descriptor; fixed array stride.
    pub abi_size: u32,
    /// Must be zero.
    pub abi_reserved: u32,
    /// Nonempty readable fixed-stride items.
    pub items: *const ZeGraphBatchItem,
    /// Between 1 and 16384.
    pub item_count: usize,
    /// Required readable input pool; borrowed only for the call.
    pub pool: *const ZeGraphValuePool,
    /// Optional controls; null means defaults, not unbounded work.
    pub control: *const ZeGraphControl,
}

/// Fixed optional pool/slot index. Presence exactly 0 or 1; absent index is zero. Index zero is legal when present.
#[derive(Clone, Copy, Debug)]
#[repr(C)]
pub struct ZeGraphOptionalIndex {
    /// Exactly zero or one.
    pub present: u32,
    /// Named array or logical slot index; never a pointer.
    pub index: u32,
}

/// Expression tag; operand references are expression indices, never source text.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[repr(u32)]
pub enum ZeGraphExpressionKind {
    /// Scalar constant from pool.values; no entity literal.
    ZeGraphExprLiteral = 0,
    /// Read slot in scope at each use.
    ZeGraphExprSlot = 1,
    /// Read declared parameter.
    ZeGraphExprParameter = 2,
    /// Unary operation on left.
    ZeGraphExprUnary = 3,
    /// Binary operation on left/right.
    ZeGraphExprBinary = 4,
    /// Static named property of left.
    ZeGraphExprProperty = 5,
    /// Static named node label predicate on left.
    ZeGraphExprHasLabel = 6,
    /// Ordered expression-child range.
    ZeGraphExprList = 7,
    /// Count or collect; optional left operand with distinct flag.
    ZeGraphExprAggregate = 8,
}

/// Unary expression semantics; all other tags reject.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[repr(u32)]
pub enum ZeGraphUnaryOperation {
    /// Three-valued logical negation.
    ZeGraphUnaryNot = 0,
    /// Checked numeric unary plus.
    ZeGraphUnaryPositive = 1,
    /// Checked numeric negation.
    ZeGraphUnaryNegate = 2,
    /// Exact null test.
    ZeGraphUnaryIsNull = 3,
    /// Exact nonnull test.
    ZeGraphUnaryIsNotNull = 4,
    /// Unicode scalar or list count.
    ZeGraphUnarySize = 5,
    /// Node label-name list.
    ZeGraphUnaryLabels = 6,
    /// Relationship type string.
    ZeGraphUnaryRelationshipType = 7,
    /// Optional stored source text.
    ZeGraphUnaryStoredText = 8,
    /// Full node identity as 32 lowercase hex digits.
    ZeGraphUnaryNodeIdText = 9,
    /// Full relationship identity as 32 lowercase hex digits.
    ZeGraphUnaryRelationshipIdText = 10,
}

/// Binary query semantics; equality is not canonical retry equality.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[repr(u32)]
pub enum ZeGraphBinaryOperation {
    /// Three-valued AND.
    ZeGraphBinaryAnd = 0,
    /// Three-valued OR.
    ZeGraphBinaryOr = 1,
    /// Three-valued XOR.
    ZeGraphBinaryXor = 2,
    /// Nullable query equality.
    ZeGraphBinaryEqual = 3,
    /// Nullable query inequality.
    ZeGraphBinaryNotEqual = 4,
    /// Nullable less comparison.
    ZeGraphBinaryLess = 5,
    /// Nullable less-or-equal comparison.
    ZeGraphBinaryLessEqual = 6,
    /// Nullable greater comparison.
    ZeGraphBinaryGreater = 7,
    /// Nullable greater-or-equal comparison.
    ZeGraphBinaryGreaterEqual = 8,
    /// Checked numeric addition.
    ZeGraphBinaryAdd = 9,
    /// Checked numeric subtraction.
    ZeGraphBinarySubtract = 10,
    /// Checked numeric multiplication.
    ZeGraphBinaryMultiply = 11,
    /// Integer truncating or floating division.
    ZeGraphBinaryDivide = 12,
    /// Remainder with dividend sign.
    ZeGraphBinaryRemainder = 13,
    /// Exact string prefix.
    ZeGraphBinaryStartsWith = 14,
    /// Exact string suffix.
    ZeGraphBinaryEndsWith = 15,
    /// Exact string containment.
    ZeGraphBinaryContains = 16,
    /// Three-valued list membership.
    ZeGraphBinaryIn = 17,
    /// Signed list indexing.
    ZeGraphBinaryIndex = 18,
}

/// Top-level aggregate forms only; nested or incorrect-context uses reject.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[repr(u32)]
pub enum ZeGraphAggregateOperation {
    /// Count rows when has_operand=0; otherwise nonnull values.
    ZeGraphAggregateCount = 0,
    /// Ordered nonnull values; has_operand must be one.
    ZeGraphAggregateCollect = 1,
}

/// Fixed expression descriptor. Fields not named by kind are zero. Aggregate uses operation, has_operand, left and distinct; unary uses operation/left; binary uses operation/left/right.
#[derive(Clone, Copy, Debug)]
#[repr(C)]
pub struct ZeGraphExpression {
    /// Exact sizeof this version-one descriptor; fixed array stride.
    pub abi_size: u32,
    /// Must be zero.
    pub abi_reserved: u32,
    /// One ZeGraphExpressionKind.
    pub kind: u32,
    /// Corresponding unary/binary/aggregate discriminant; zero otherwise.
    pub operation: u32,
    /// First expression operand; used by unary/binary/property/label/aggregate.
    pub left: u32,
    /// Second expression operand for Binary; zero otherwise.
    pub right: u32,
    /// Aggregate only: 0 for count(*), 1 for an operand.
    pub has_operand: u32,
    /// Aggregate only: 0 or 1.
    pub distinct: u32,
    /// Literal: pool value index. Slot: logical slot ID. Parameter: declaration index.
    pub value: u32,
    /// Must be zero.
    pub reserved: u32,
    /// Property/HasLabel name bytes in plan pool.
    pub name: ZeGraphRange,
    /// List only: range in plan.expression_children.
    pub children: ZeGraphRange,
}

/// One output binding; expression is evaluated in the input scope.
#[derive(Clone, Copy, Debug)]
#[repr(C)]
pub struct ZeGraphProjection {
    /// Exact sizeof this version-one descriptor; fixed array stride.
    pub abi_size: u32,
    /// Must be zero.
    pub abi_reserved: u32,
    /// Logical u32 slot ID, not a row offset; per-scope width at most 256.
    pub slot: u32,
    /// Expression index.
    pub expression: u32,
}

/// One ordered comparison key; later keys break ties.
#[derive(Clone, Copy, Debug)]
#[repr(C)]
pub struct ZeGraphSortKey {
    /// Exact sizeof this version-one descriptor; fixed array stride.
    pub abi_size: u32,
    /// Must be zero.
    pub abi_reserved: u32,
    /// Expression in current scope.
    pub expression: u32,
    /// Exactly zero or one.
    pub descending: u32,
}

/// Parameter declaration; nested entity values are forbidden too.
#[derive(Clone, Copy, Debug)]
#[repr(C)]
pub struct ZeGraphParameter {
    /// Exact sizeof this version-one descriptor; fixed array stride.
    pub abi_size: u32,
    /// Must be zero.
    pub abi_reserved: u32,
    /// Exact UTF-8 name bytes in plan pool.
    pub name: ZeGraphRange,
    /// Nonzero mask: Null1 Bool2 I644 F648 String16 List128; bits32/64 forbidden.
    pub kinds: u32,
    /// Must be zero.
    pub reserved: u32,
}

/// Named nonentity parameter binding; no extra/duplicate/missing names.
#[derive(Clone, Copy, Debug)]
#[repr(C)]
pub struct ZeGraphParameterValue {
    /// Exact sizeof this version-one descriptor; fixed array stride.
    pub abi_size: u32,
    /// Must be zero.
    pub abi_reserved: u32,
    /// Name bytes in the request parameter pool.
    pub name: ZeGraphRange,
    /// Value index in that parameter pool.
    pub value: u32,
    /// Must be zero.
    pub reserved: u32,
}

/// Search tier. Absent preference remains distinct from explicit Auto.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[repr(u32)]
pub enum ZeGraphTier {
    /// Explicit automatic tier.
    ZeGraphTierAuto = 0,
    /// Exhaustive original-f32 scoring.
    ZeGraphTierExact = 1,
    /// Quantized scan; coverage and precision are reported separately.
    ZeGraphTierScan = 2,
    /// Explicit structured ANN tier; not a textual approximate Cypher mode.
    ZeGraphTierGraph = 3,
}

/// Once-per-query search source.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[repr(u32)]
pub enum ZeGraphSearchKind {
    /// Vector distance ascending.
    ZeGraphSearchVector = 0,
    /// Lexical BM25 descending.
    ZeGraphSearchText = 1,
    /// Fused score descending, independent nullable component yields.
    ZeGraphSearchHybrid = 2,
}

/// Typed vector/lexical/fusion options. Irrelevant options reject; defaults and absent preferences retain existing Store semantics. Window is a separate evaluated expression in ZeGraphSearch.
#[derive(Clone, Copy, Debug)]
#[repr(C)]
pub struct ZeGraphSearchOptions {
    /// Exact sizeof this version-one descriptor; fixed array stride.
    pub abi_size: u32,
    /// Must be zero.
    pub abi_reserved: u32,
    /// 0 SIFT-class or 1 angular; used by structured Graph/Auto routing.
    pub graph_profile: u32,
    /// 0 adaptive or explicit positive width within admitted work limits.
    pub graph_ef: u32,
    /// Deterministic query preparation seed.
    pub graph_seed: u64,
    /// Only bit0 last-as-prefix is accepted; zero otherwise.
    pub lexical_flags: u32,
    /// 0 default, 1 original-f32 rescore; never implies exhaustive coverage.
    pub rescore: u32,
    /// Exactly 0 or 1; absent alpha bits are positive zero.
    pub has_alpha: u32,
    /// Exactly 0 or 1; query-level alpha rules, ignored by explicit alpha.
    pub rules_enabled: u32,
    /// Explicit finite convex weight in `[0,1]`.
    pub alpha: f64,
    /// Exactly 0 or 1; absent max_rounds is zero.
    pub has_max_rounds: u32,
    /// Must be zero.
    pub reserved: u32,
    /// Explicit widening limit; zero means full-list strategy within shared caps.
    pub max_rounds: u64,
}

/// Eager uncorrelated invocation. Runtime validates evaluated k/window before work; each syntactic call remains required even under LIMIT 0. Eligibility is a query-local typed set slot, never external IDs.
#[derive(Clone, Copy, Debug)]
#[repr(C)]
pub struct ZeGraphSearch {
    /// Exact sizeof this version-one descriptor; fixed array stride.
    pub abi_size: u32,
    /// Must be zero.
    pub abi_reserved: u32,
    /// One ZeGraphSearchKind.
    pub kind: u32,
    /// Unique source-order ID; eager_searches must retain every call in order.
    pub call_id: u32,
    /// Vector expression for Vector/Hybrid; absent for Text.
    pub vector: ZeGraphOptionalIndex,
    /// Text expression for Text/Hybrid; absent for Vector.
    pub text: ZeGraphOptionalIndex,
    /// Expression yielding positive I64 <=4096.
    pub k: u32,
    /// Exactly 0 or 1; absent tier field must be zero.
    pub has_tier: u32,
    /// One ZeGraphTier; text-only tier must be absent.
    pub tier: u32,
    /// Must be zero.
    pub reserved: u32,
    /// Logical eligible-set slot, produced by EligibleSet operator; absence means AllIndexed.
    pub eligible_set: ZeGraphOptionalIndex,
    /// Optional expression yielding nonnegative I64 <=65536; zero is an explicit tightened allowance, not default.
    pub window: ZeGraphOptionalIndex,
    /// Node output slot.
    pub node_slot: u32,
    /// Distance for Vector, score for Text/Hybrid.
    pub score_slot: u32,
    /// Hybrid only: optional nullable original component distance output slot.
    pub vector_distance_slot: ZeGraphOptionalIndex,
    /// Hybrid only: optional nullable BM25 output slot.
    pub lexical_score_slot: ZeGraphOptionalIndex,
    /// Optional typed options; null means defaults.
    pub options: *const ZeGraphSearchOptions,
}

/// Typed graph operators; no SQL/Cypher text or serialized opcode stream.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[repr(u32)]
pub enum ZeGraphOperatorKind {
    /// Zero inputs, one empty row.
    ZeGraphOpUnit = 0,
    /// Two inputs, optional combined-scope predicate; shared slots are equality keys.
    ZeGraphOpJoin = 1,
    /// One input, query-equivalence whole-row deduplication.
    ZeGraphOpDistinct = 2,
    /// One input, sort_keys range.
    ZeGraphOpSort = 3,
    /// Zero inputs, node_slot and optional label name.
    ZeGraphOpScanNodes = 4,
    /// One input, drain/freeze full mutation input.
    ZeGraphOpEager = 5,
    /// One immediate Eager input, ordered mutations range.
    ZeGraphOpMutate = 6,
    /// One input, projections group keys and aggregates output range.
    ZeGraphOpAggregate = 7,
    /// Zero row inputs, search descriptor index; eligibility dependency is explicit.
    ZeGraphOpSearch = 8,
    /// One input, u64 offset and optional limit.
    ZeGraphOpOffsetLimit = 9,
    /// Zero inputs, node_slot and nonzero node_id.
    ZeGraphOpLookupNode = 10,
    /// Zero inputs, relationship_slot and nonzero relationship_id.
    ZeGraphOpLookupRelationship = 11,
    /// Zero inputs, node_slot output, entity_kind, namespace name and key expression.
    ZeGraphOpLookupKey = 12,
    /// One input, source/node/relationship slots, direction, OR type range and pattern.
    ZeGraphOpExpand = 13,
    /// One input, Expand fields plus inclusive path_min/path_max and optional per-edge predicate.
    ZeGraphOpBoundedExpand = 14,
    /// Two inputs, correlated optional predicate before null extension.
    ZeGraphOpOptionalApply = 15,
    /// One input, projections range.
    ZeGraphOpProject = 16,
    /// One input, projections and mandatory scope barrier.
    ZeGraphOpWith = 17,
    /// One input, required predicate.
    ZeGraphOpFilter = 18,
    /// One input, completed output boundary.
    ZeGraphOpCollect = 19,
    /// One input, source_slot node values to immutable deduplicated same-view set_slot; cardinality <=524288.
    ZeGraphOpEligibleSet = 20,
}

/// Fixed typed operator descriptor. Only kind-documented fields are active; all others zero. Inputs and range fields refer to named plan arrays. Search may have one eligibility dependency in inputs when eligible_set is present; no row correlation.
#[derive(Clone, Copy, Debug)]
#[repr(C)]
pub struct ZeGraphOperator {
    /// Exact sizeof this version-one descriptor; fixed array stride.
    pub abi_size: u32,
    /// Must be zero.
    pub abi_reserved: u32,
    /// One ZeGraphOperatorKind.
    pub kind: u32,
    /// LookupKey only: one ZeGraphEntityKind.
    pub entity_kind: u32,
    /// Ordered operator indices in plan.inputs; exact arity depends on kind.
    pub inputs: ZeGraphRange,
    /// Filter required, Join/OptionalApply optional; expression index.
    pub predicate: ZeGraphOptionalIndex,
    /// Expand/BoundedExpand/EligibleSet input node slot.
    pub source_slot: u32,
    /// Scan/lookup/expand node or LookupKey entity output.
    pub node_slot: u32,
    /// LookupRelationship/Expand entity or BoundedExpand list output.
    pub relationship_slot: u32,
    /// EligibleSet output logical set ID; not a row value or external handle.
    pub set_slot: u32,
    /// Expand/BoundedExpand: 0 outgoing, 1 incoming, 2 either.
    pub direction: u32,
    /// Expand/BoundedExpand MATCH uniqueness scope; origins preserved across joins.
    pub pattern: u32,
    /// Scan optional label or LookupKey namespace bytes in pool.
    pub name: ZeGraphRange,
    /// Scan only: 0/1; LookupKey name is required and has_name=1.
    pub has_name: u32,
    /// LookupKey only: string key expression index.
    pub key_expression: u32,
    /// LookupNode only; zero otherwise.
    pub node_id: ZeNodeId,
    /// LookupRelationship only; zero otherwise.
    pub relationship_id: ZeRelId,
    /// OR-ed type-name ranges in pool.names; zero count means unconstrained.
    pub relationship_types: ZeGraphRange,
    /// BoundedExpand inclusive lower bound, 0..16.
    pub path_min: u32,
    /// BoundedExpand inclusive upper bound, >=min and <=16.
    pub path_max: u32,
    /// BoundedExpand per-edge expression, evaluated using edge_slot plus input scope.
    pub edge_predicate: ZeGraphOptionalIndex,
    /// Temporary relationship slot visible only in edge_predicate; zero if absent.
    pub edge_slot: u32,
    /// Search only: plan.searches index.
    pub search: u32,
    /// Project/With output or Aggregate group keys in plan.projections.
    pub projections: ZeGraphRange,
    /// Aggregate output projections in plan.projections.
    pub aggregates: ZeGraphRange,
    /// Sort only: plan.sort_keys range.
    pub sort_keys: ZeGraphRange,
    /// Mutate only: plan.mutations range.
    pub mutations: ZeGraphRange,
    /// OffsetLimit skipped rows.
    pub offset: u64,
    /// OffsetLimit maximum rows if has_limit=1; zero otherwise.
    pub limit: u64,
    /// OffsetLimit only: exactly 0 or 1.
    pub has_limit: u32,
    /// Must be zero.
    pub reserved: u32,
}

/// Ordered structured query mutation items; evaluated in progressive overlay.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[repr(u32)]
pub enum ZeGraphMutationKind {
    /// Fresh node bound to output with label set.
    ZeGraphMutationCreateNode = 0,
    /// Fresh relationship bound to output with source/target expressions and fixed type name.
    ZeGraphMutationCreateRelationship = 1,
    /// Remove named property from entity.
    ZeGraphMutationRemoveProperty = 2,
    /// Add/remove named label using present.
    ZeGraphMutationSetLabel = 3,
    /// Delete entity using detach for nodes only.
    ZeGraphMutationDelete = 4,
    /// Assign named value expression; null removes property.
    ZeGraphMutationSetProperty = 5,
}

/// One query mutation. Inactive fields zero. No retry/upsert implication; images and identity allocation belong to staging.
#[derive(Clone, Copy, Debug)]
#[repr(C)]
pub struct ZeGraphMutation {
    /// Exact sizeof this version-one descriptor; fixed array stride.
    pub abi_size: u32,
    /// Must be zero.
    pub abi_reserved: u32,
    /// One ZeGraphMutationKind.
    pub kind: u32,
    /// CreateNode/CreateRelationship destination slot.
    pub output: u32,
    /// Entity expression for noncreate items.
    pub entity: u32,
    /// SetProperty value expression.
    pub value: u32,
    /// CreateRelationship source node expression.
    pub source: u32,
    /// CreateRelationship target node expression.
    pub target: u32,
    /// SetLabel only: exactly 0 or 1.
    pub present: u32,
    /// Delete only: exactly 0 or 1.
    pub detach: u32,
    /// Property/label/type name in plan pool.bytes.
    pub name: ZeGraphRange,
    /// CreateNode only: label ranges in plan pool.names.
    pub labels: ZeGraphRange,
}

/// Fixed-stride typed plan arenas. ABI shape validation is not DAG/type/scope/liveness admission. Runtime must converge on core validation and reject unsupported descriptors, never silently drop fields.
#[derive(Clone, Copy, Debug)]
#[repr(C)]
pub struct ZeGraphPlan {
    /// Exact sizeof this version-one descriptor; fixed array stride.
    pub abi_size: u32,
    /// Must be zero.
    pub abi_reserved: u32,
    /// Row-producing operator index.
    pub root: u32,
    /// Must be zero.
    pub reserved: u32,
    /// At most 4096 DAG nodes. Null only at zero count.
    pub operators: *const ZeGraphOperator,
    /// Accessible elements of operators.
    pub operator_count: usize,
    /// At most 4096 expressions; maximum depth 64. Null only at zero count.
    pub expressions: *const ZeGraphExpression,
    /// Accessible elements of expressions.
    pub expression_count: usize,
    /// Ordered operator dependency indices. Null only at zero count.
    pub inputs: *const u32,
    /// Accessible elements of inputs.
    pub input_count: usize,
    /// Ordered list expression indices. Null only at zero count.
    pub expression_children: *const u32,
    /// Accessible elements of expression_children.
    pub expression_child_count: usize,
    /// Output/group bindings. Null only at zero count.
    pub projections: *const ZeGraphProjection,
    /// Accessible elements of projections.
    pub projection_count: usize,
    /// Ordered comparison keys. Null only at zero count.
    pub sort_keys: *const ZeGraphSortKey,
    /// Accessible elements of sort_keys.
    pub sort_key_count: usize,
    /// Progressive overlay mutation descriptors. Null only at zero count.
    pub mutations: *const ZeGraphMutation,
    /// Accessible elements of mutations.
    pub mutation_count: usize,
    /// Unique nonentity parameter declarations. Null only at zero count.
    pub parameters: *const ZeGraphParameter,
    /// Accessible elements of parameters.
    pub parameter_count: usize,
    /// At most eight eager source descriptors. Null only at zero count.
    pub searches: *const ZeGraphSearch,
    /// Accessible elements of searches.
    pub search_count: usize,
    /// Search operator indices in validated source order, regardless of row reachability. Null only at zero count.
    pub eager_searches: *const u32,
    /// Accessible elements of eager_searches.
    pub eager_search_count: usize,
    /// Required literals and name pool; no entity literals.
    pub pool: *const ZeGraphValuePool,
}

/// Query interpretation declaration. Query tower/alignment metadata does not alter the document interpretation stored at open.
#[derive(Clone, Copy, Debug)]
#[repr(C)]
pub struct ZeGraphQueryOptions {
    /// Exact sizeof this version-one descriptor; fixed array stride.
    pub abi_size: u32,
    /// Must be zero.
    pub abi_reserved: u32,
    /// Optional declared query-tower identity; compatibility checked with document tower.
    pub query_tower: *const ZeEmbeddingTower,
    /// Optional opaque alignment digest bytes; empty means absent.
    pub alignment_digest: ZeGraphBytes,
    /// Optional typed memory/work tightening; null uses existing hard defaults.
    pub limits: *const ZeGraphQueryLimits,
}

use crate::ZeEmbeddingTower;

/// Synchronous structured query; caller buffers borrowed only until return. No result can retain a view or caller pointer.
#[derive(Clone, Copy, Debug)]
#[repr(C)]
pub struct ZeGraphQueryRequest {
    /// Exact sizeof this version-one descriptor; fixed array stride.
    pub abi_size: u32,
    /// Must be zero.
    pub abi_reserved: u32,
    /// Required typed plan.
    pub plan: *const ZeGraphPlan,
    /// Named bindings; null only at count zero.
    pub parameters: *const ZeGraphParameterValue,
    /// Number of bindings.
    pub parameter_count: usize,
    /// Required when any binding exists; otherwise optional.
    pub parameter_pool: *const ZeGraphValuePool,
    /// Optional query options.
    pub options: *const ZeGraphQueryOptions,
    /// Optional cancellation/deadline controls.
    pub control: *const ZeGraphControl,
}

/// Explicit graph create/open distinction; no legacy fallback or weaker durability.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[repr(u32)]
pub enum ZeGraphOpenMode {
    /// Create a fresh native graph store; never replace incompatible or existing contents.
    ZeGraphOpenCreate = 0,
    /// Open existing native graph with exclusive writer lock and Full durability.
    ZeGraphOpenReadWrite = 1,
    /// Open existing native graph with shared lock and no recovery mutation.
    ZeGraphOpenReadOnly = 2,
}

/// Graph-only open declaration. Document-tower identity is persisted; query tower/alignment is intentionally absent. ZE-69/coordinator own locks, format admission and initialization.
#[derive(Clone, Copy, Debug)]
#[repr(C)]
pub struct ZeGraphOpenRequest {
    /// Exact sizeof this version-one descriptor; fixed array stride.
    pub abi_size: u32,
    /// Must be zero.
    pub abi_reserved: u32,
    /// Nonempty UTF-8 filesystem path; embedded NUL forbidden.
    pub path: ZeGraphBytes,
    /// One ZeGraphOpenMode.
    pub mode: u32,
    /// 0 existing general-purpose tokenizer; unknown profiles reject.
    pub tokenizer_profile: u32,
    /// Null declares graph without vector space; otherwise exact optional document interpretation.
    pub document_tower: *const ZeEmbeddingTower,
    /// Close drain grace period in milliseconds, using existing lifecycle semantics.
    pub reader_drain_timeout_ms: u64,
    /// Nonzero shared store owned-capacity ceiling, at most 256 MiB; includes concurrent graph work.
    pub max_resident_bytes: u64,
    /// Optional cancellation/deadline for bounded create/open work.
    pub control: *const ZeGraphControl,
}

/// Immutable incoming-reference policy, accepted only during graph creation.
#[derive(Clone, Copy, Debug)]
#[repr(C)]
pub struct ZeGraphRelationshipType {
    /// Exact sizeof this descriptor.
    pub abi_size: u32,
    /// Must be zero.
    pub abi_reserved: u32,
    /// Exact UTF-8 relationship type name.
    pub name: ZeGraphBytes,
    /// 1 restrict, 2 cascade. Other values reject before creating any files.
    pub on_delete: u32,
    /// Must be zero.
    pub reserved: u32,
}

/// Outer compiler request; only typed plans enter core. Invalid/unsupported syntax returns owned bounded diagnostics and no effects.
#[derive(Clone, Copy, Debug)]
#[repr(C)]
pub struct ZeGraphCypherRequest {
    /// Exact sizeof this version-one descriptor; fixed array stride.
    pub abi_size: u32,
    /// Must be zero.
    pub abi_reserved: u32,
    /// One UTF-8 query text, at most 64 KiB; no binary plan encoding.
    pub query: ZeGraphBytes,
    /// Named typed bindings; null only at zero count.
    pub parameters: *const ZeGraphParameterValue,
    /// Number of parameter bindings.
    pub parameter_count: usize,
    /// Required if bindings exist; otherwise optional.
    pub parameter_pool: *const ZeGraphValuePool,
    /// Optional query interpretation.
    pub options: *const ZeGraphQueryOptions,
    /// Optional controls covering parsing through completion.
    pub control: *const ZeGraphControl,
    /// Optional complete compiler limit tightening; null selects existing defaults.
    pub compile_limits: *const ZeGraphCompileLimits,
}

/// Single-admission typed entity read. Returns one column, one row per requested ID, explicit Null for missing IDs; no second snapshot or automatic query follow-up.
#[derive(Clone, Copy, Debug)]
#[repr(C)]
pub struct ZeGraphGetNodesRequest {
    /// Exact sizeof this version-one descriptor; fixed array stride.
    pub abi_size: u32,
    /// Must be zero.
    pub abi_reserved: u32,
    /// Nonzero store-local IDs; null only at zero count.
    pub ids: *const ZeNodeId,
    /// Input count; completed result bounds apply, order and duplicates preserved.
    pub id_count: usize,
    /// 0/1 explicit source text selection; absent differs from selected empty.
    pub include_text: u32,
    /// 0/1 explicit original vector selection; dimension validated.
    pub include_vector: u32,
    /// Optional cancellation/deadline controls.
    pub control: *const ZeGraphControl,
    /// Optional typed memory/work tightening; null uses existing hard defaults.
    pub limits: *const ZeGraphQueryLimits,
}

/// Single-admission typed entity read. Returns one column, one row per requested ID, explicit Null for missing IDs; no second snapshot or automatic query follow-up.
#[derive(Clone, Copy, Debug)]
#[repr(C)]
pub struct ZeGraphGetRelsRequest {
    /// Exact sizeof this version-one descriptor; fixed array stride.
    pub abi_size: u32,
    /// Must be zero.
    pub abi_reserved: u32,
    /// Nonzero store-local IDs; null only at zero count.
    pub ids: *const ZeRelId,
    /// Input count; completed result bounds apply, order and duplicates preserved.
    pub id_count: usize,
    /// Optional cancellation/deadline controls.
    pub control: *const ZeGraphControl,
    /// Optional typed memory/work tightening; null uses existing hard defaults.
    pub limits: *const ZeGraphQueryLimits,
}

/// Coordinator-owned durable outcome, also present on errors when output descriptor is valid.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[repr(u32)]
pub enum ZeGraphDisposition {
    /// Read-only operation; no write attempt.
    ZeGraphDispositionNotApplicable = 0,
    /// Coordinator established definite no durable effects.
    ZeGraphDispositionNotCommitted = 1,
    /// Known durable commit; changed generation present.
    ZeGraphDispositionCommitted = 2,
    /// All operations replay; no new changed generation, original per-item generations retained.
    ZeGraphDispositionReplayed = 3,
    /// Successful no durable change, including empty matched input; admitted generation present.
    ZeGraphDispositionNoOp = 4,
    /// Durable outcome not established; no guessed new IDs or changed generation.
    ZeGraphDispositionIndeterminate = 5,
}

/// One successful structured item outcome; only Committed/Replayed dispositions. Receipts do not authorize generic Cypher retry.
#[derive(Clone, Copy, Debug)]
#[repr(C)]
pub struct ZeGraphReceipt {
    /// Exact sizeof this version-one descriptor; fixed array stride.
    pub abi_size: u32,
    /// Must be zero.
    pub abi_reserved: u32,
    /// Zero-based original batch item index.
    pub item: u32,
    /// One ZeGraphEntityKind.
    pub entity_kind: u32,
    /// Committed for new change or Replayed for exact retry.
    pub disposition: u32,
    /// 0/1 deletion outcome; identity denotes affected incarnation.
    pub deleted: u32,
    /// Nonzero only for node outcome.
    pub node: ZeNodeId,
    /// Nonzero only for relationship outcome.
    pub relationship: ZeRelId,
    /// Installed positive entity/deletion revision.
    pub revision: u64,
    /// Original changed generation; mixed batches may retain older replay generations.
    pub generation: u64,
}

/// One completed result column; order and duplicate display names are preserved.
#[derive(Clone, Copy, Debug)]
#[repr(C)]
pub struct ZeGraphColumn {
    /// Exact sizeof this version-one descriptor; fixed array stride.
    pub abi_size: u32,
    /// Must be zero.
    pub abi_reserved: u32,
    /// UTF-8 name bytes in response.pool.bytes.
    pub name: ZeGraphRange,
    /// Nonzero bitmask Null1 Bool2 I644 F648 String16 Node32 Rel64 List128.
    pub kinds: u32,
    /// Must be zero.
    pub reserved: u32,
}

/// Bounded owned error diagnostic, separate from global mutable last-error. Ranges refer to the query source or response pool as named; no borrowed caller strings survive.
#[derive(Clone, Copy, Debug)]
#[repr(C)]
pub struct ZeGraphDiagnostic {
    /// Exact sizeof this version-one descriptor; fixed array stride.
    pub abi_size: u32,
    /// Must be zero.
    pub abi_reserved: u32,
    /// One append-only ZeErrorCode numeric value.
    pub code: i32,
    /// Must be zero.
    pub reserved: u32,
    /// Optional originating plan operator index.
    pub operator_index: ZeGraphOptionalIndex,
    /// Exactly zero or one; absent source range is zero.
    pub has_source_span: u32,
    /// Must be zero.
    pub source_reserved: u32,
    /// Byte range in original UTF-8 query source, not character offsets.
    pub source_span: ZeGraphRange,
    /// Owned diagnostic UTF-8 bytes in response.pool.bytes.
    pub message: ZeGraphRange,
}

/// Precision of computed scores, independent of candidate coverage.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[repr(u32)]
pub enum ZeGraphScorePrecision {
    /// No vector component evaluated (for example lexical only).
    ZeGraphPrecisionNotApplicable = 0,
    /// Original f32 vector domain; does not certify candidate coverage.
    ZeGraphPrecisionOriginal = 1,
    /// Quantized scores without full original rescore.
    ZeGraphPrecisionQuantized = 2,
    /// Different actual vector scoring precisions are retained.
    ZeGraphPrecisionMixed = 3,
}

/// Candidate coverage evidence; graph expansion cannot upgrade approximate seeds.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[repr(u32)]
pub enum ZeGraphCandidateCoverage {
    /// Exhaustive or valid certified eligible top-k.
    ZeGraphCoverageExact = 0,
    /// Non-exhaustive candidate source without complete certificate.
    ZeGraphCoverageApproximate = 1,
}

/// Proven empty-leg reasons, independent of an empty approximate candidate window.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[repr(u32)]
pub enum ZeGraphLegState {
    /// Modality not requested.
    ZeGraphLegNotRequested = 0,
    /// Eligible modality has candidates/matches.
    ZeGraphLegNonempty = 1,
    /// No live indexed modality population.
    ZeGraphLegNoIndexedPopulation = 2,
    /// Indexed population exists but eligibility intersection is empty.
    ZeGraphLegNoEligibleMembers = 3,
    /// Eligible indexed text exists but lexical query has no matches.
    ZeGraphLegNoQueryMatches = 4,
}

/// Actual cumulative work categories, matching runtime ownership; no TCK side-effect counters.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[repr(u32)]
pub enum ZeGraphWorkKind {
    /// Actual OperatorRows under the runtime counter contract.
    ZeGraphWorkOperatorRows = 0,
    /// Actual AdjacencyEntries under the runtime counter contract.
    ZeGraphWorkAdjacencyEntries = 1,
    /// Actual Expressions under the runtime counter contract.
    ZeGraphWorkExpressions = 2,
    /// Actual HashProbes under the runtime counter contract.
    ZeGraphWorkHashProbes = 3,
    /// Actual CompletedRows under the runtime counter contract.
    ZeGraphWorkCompletedRows = 4,
    /// Actual CompletedBytes under the runtime counter contract.
    ZeGraphWorkCompletedBytes = 5,
    /// Actual PreparedPayloadBytes under the runtime counter contract.
    ZeGraphWorkPreparedPayloadBytes = 6,
    /// Actual CompletedAbiBytes under the runtime counter contract.
    ZeGraphWorkCompletedAbiBytes = 7,
    /// Actual VectorCoordinates under the runtime counter contract.
    ZeGraphWorkVectorCoordinates = 8,
    /// Actual VectorBytes under the runtime counter contract.
    ZeGraphWorkVectorBytes = 9,
    /// Actual LexicalPostings under the runtime counter contract.
    ZeGraphWorkLexicalPostings = 10,
    /// Actual LexicalBlocks under the runtime counter contract.
    ZeGraphWorkLexicalBlocks = 11,
    /// Actual SearchInvocations under the runtime counter contract.
    ZeGraphWorkSearchInvocations = 12,
    /// Actual Lookups under the runtime counter contract.
    ZeGraphWorkLookups = 13,
    /// Actual Scans under the runtime counter contract.
    ZeGraphWorkScans = 14,
    /// Actual Paths under the runtime counter contract.
    ZeGraphWorkPaths = 15,
    /// Actual RowsIn under the runtime counter contract.
    ZeGraphWorkRowsIn = 16,
    /// Actual RowsOut under the runtime counter contract.
    ZeGraphWorkRowsOut = 17,
    /// Actual JoinProbes under the runtime counter contract.
    ZeGraphWorkJoinProbes = 18,
    /// Actual GroupKeys under the runtime counter contract.
    ZeGraphWorkGroupKeys = 19,
    /// Actual EligibilityEntries under the runtime counter contract.
    ZeGraphWorkEligibilityEntries = 20,
    /// Actual CopiedBytes under the runtime counter contract.
    ZeGraphWorkCopiedBytes = 21,
    /// Monotone peak actual charged capacity.
    ZeGraphWorkPeakOwnedBytes = 22,
}

/// One actual counter; global totals include all invocations and normalization passes. Capacity counters are bytes, never logical lengths.
#[derive(Clone, Copy, Debug)]
#[repr(C)]
pub struct ZeGraphWorkCounter {
    /// Exact sizeof this version-one descriptor; fixed array stride.
    pub abi_size: u32,
    /// Must be zero.
    pub abi_reserved: u32,
    /// One ZeGraphWorkKind.
    pub kind: u32,
    /// Must be zero.
    pub reserved: u32,
    /// Actual measured count; never requested or estimated work.
    pub value: u64,
}

/// One eager invocation report retained through projections/aggregations, even with zero result rows. Component yields distinguish absent membership from numeric zero; cross-scoring is complete for retained candidates.
#[derive(Clone, Copy, Debug)]
#[repr(C)]
pub struct ZeGraphSearchReport {
    /// Exact sizeof this version-one descriptor; fixed array stride.
    pub abi_size: u32,
    /// Must be zero.
    pub abi_reserved: u32,
    /// Unique source-order invocation ID.
    pub call_id: u32,
    /// One ZeGraphSearchKind.
    pub kind: u32,
    /// Same admitted generation as response.
    pub generation: u64,
    /// 0/1, preserving omitted preference.
    pub has_requested_tier: u32,
    /// One ZeGraphTier when present; zero otherwise.
    pub requested_tier: u32,
    /// 0/1; absent for lexical-only execution.
    pub has_actual_tier: u32,
    /// Exact/Scan/Graph actual vector route; zero if absent.
    pub actual_tier: u32,
    /// One ZeGraphScorePrecision; original scores do not imply exact coverage.
    pub precision: u32,
    /// One ZeGraphCandidateCoverage.
    pub coverage: u32,
    /// One ZeGraphLegState.
    pub vector_leg: u32,
    /// One ZeGraphLegState.
    pub lexical_leg: u32,
    /// 0/1, selected stored document interpretation.
    pub has_document_epoch: u32,
    /// 0/1, declared/effective query interpretation.
    pub has_query_epoch: u32,
    /// 0/1, selected lexical interpretation.
    pub has_tokenizer_epoch: u32,
    /// 0/1: every retained hybrid candidate has all present components evaluated.
    pub cross_score_complete: u32,
    /// Selected document epoch; zero if absent.
    pub document_epoch: u64,
    /// Selected query epoch; zero if absent.
    pub query_epoch: u64,
    /// Selected analyzer epoch; zero if absent.
    pub tokenizer_epoch: u64,
    /// Effective query-level vector weight, finite in `[0,1]`; no per-node renormalization.
    pub effective_alpha: f64,
    /// Actual scoring-anchor policy version.
    pub normalization_version: u32,
    /// Actual alpha/rule policy version.
    pub rules_version: u32,
    /// Actual retained candidate union size, deduplicated by full identity/version.
    pub candidate_count: u64,
    /// Actual retained candidates whose present modalities were evaluated.
    pub cross_scored_count: u64,
    /// Actual fallbacks; zero does not certify exactness.
    pub fallback_count: u64,
    /// Invocation-specific rows in response.work; global_work names separate cumulative totals.
    pub work: ZeGraphRange,
}

/// Completed root descriptor. A valid output is emptied before request/handle validation; every nested pointer remains immutable until matching future response_free, including after store close. ABI arena <=4 MiB; registry/control capacity separately charged. This declaration does not implement ownership or exports.
#[derive(Clone, Copy, Debug)]
#[repr(C)]
pub struct ZeGraphResponse {
    /// Exact sizeof this version-one descriptor; fixed array stride.
    pub abi_size: u32,
    /// Must be zero.
    pub abi_reserved: u32,
    /// Opaque registry generation; zero for an empty unowned response. ZE-68 supplies registration/free.
    pub owner_token: u64,
    /// One ZeGraphDisposition; meaningful even on nonzero status.
    pub disposition: u32,
    /// 0/1; failure before admission leaves absent.
    pub has_admitted_generation: u32,
    /// Single admitted view generation; zero if absent.
    pub admitted_generation: u64,
    /// 0/1; only known Committed outcome may expose a new generation.
    pub has_changed_generation: u32,
    /// Must be zero.
    pub reserved: u32,
    /// Known changed generation; zero if absent.
    pub changed_generation: u64,
    /// Complete row count <=65536; rows preserve bag multiplicity.
    pub row_count: usize,
    /// Immutable result columns; null only at zero count.
    pub columns: *const ZeGraphColumn,
    /// Columns per row, <=256.
    pub column_count: usize,
    /// Row-major indices into pool.values; explicit Null cells represent missing values.
    pub cells: *const u32,
    /// Checked row_count * column_count; no partially populated rows.
    pub cell_count: usize,
    /// Root-owned immutable value/name/entity payload pools, independent of caller/store lifetimes.
    pub pool: ZeGraphValuePool,
    /// Per-item outcomes; no guessed IDs on Indeterminate. Null only at zero count.
    pub receipts: *const ZeGraphReceipt,
    /// Initialized elements in receipts.
    pub receipt_count: usize,
    /// Every executed eager search report in source order. Null only at zero count.
    pub reports: *const ZeGraphSearchReport,
    /// Initialized elements in reports.
    pub report_count: usize,
    /// Bounded owned diagnostics, including on errors. Null only at zero count.
    pub diagnostics: *const ZeGraphDiagnostic,
    /// Initialized elements in diagnostics.
    pub diagnostic_count: usize,
    /// Actual counter rows; global and per-call ranges distinct. Null only at zero count.
    pub work: *const ZeGraphWorkCounter,
    /// Initialized elements in work.
    pub work_count: usize,
    /// Whole-request cumulative counters in work; includes all preparation and searches.
    pub global_work: ZeGraphRange,
}

/// Explicit tightened work allowance, including zero. Unknown/duplicate categories and widening hard limits reject. Omitted categories retain current hard defaults.
#[derive(Clone, Copy, Debug)]
#[repr(C)]
pub struct ZeGraphWorkLimit {
    /// Exact sizeof this version-one descriptor; fixed array stride.
    pub abi_size: u32,
    /// Must be zero.
    pub abi_reserved: u32,
    /// One runtime ZeGraphWorkKind 0..21; PeakOwnedBytes is not a work allowance.
    pub kind: u32,
    /// Must be zero.
    pub reserved: u32,
    /// Inclusive maximum actual cumulative units; zero is a real allowance.
    pub limit: u64,
}

/// Optional query-local tightening, inside shared store accounting; these declarations never prove actual reservations or allocator ownership.
#[derive(Clone, Copy, Debug)]
#[repr(C)]
pub struct ZeGraphQueryLimits {
    /// Exact sizeof this version-one descriptor; fixed array stride.
    pub abi_size: u32,
    /// Must be zero.
    pub abi_reserved: u32,
    /// 0/1 explicit memory ceiling presence; zero ceiling is permitted and execution may fail to reserve even its context.
    pub has_query_bytes: u32,
    /// Must be zero.
    pub reserved: u32,
    /// When present, actual retained-capacity ceiling <=24 MiB; zero if absent.
    pub query_bytes: u64,
    /// At most one row per category; null only at zero count.
    pub work: *const ZeGraphWorkLimit,
    /// Between zero and 22; bounded validation before execution.
    pub work_count: usize,
}

/// Full caller-tightened outer compiler limits. Null request pointer selects defaults; when present every field is explicit, including zero. All compiler capacities still count inside the same query budget.
#[derive(Clone, Copy, Debug)]
#[repr(C)]
pub struct ZeGraphCompileLimits {
    /// Exact sizeof this version-one descriptor; fixed array stride.
    pub abi_size: u32,
    /// Must be zero.
    pub abi_reserved: u32,
    /// UTF-8 source bytes <=65536.
    pub text_bytes: u32,
    /// Lexical tokens <=8192.
    pub tokens: u32,
    /// AST nodes <=4096.
    pub ast_nodes: u32,
    /// Nesting depth <=64.
    pub depth: u32,
    /// Named parameters <=256.
    pub parameters: u32,
    /// Projected columns per scope <=256.
    pub columns: u32,
    /// Query list nesting <=16.
    pub list_depth: u32,
    /// Finite path upper bound <=16.
    pub path_hops: u32,
}

/// Per-open graph writer maintenance policy.
#[derive(Clone, Copy, Debug)]
#[repr(C)]
pub struct ZeGraphMaintenancePolicy {
    /// Exact sizeof this descriptor.
    pub abi_size: u32,
    /// 0 disables automatic maintenance; 1 enables it.
    pub automatic: u32,
    /// Trigger after this many committed artifact bytes; at least 1 MiB.
    pub reclaim_after_bytes: u64,
}

/// Owned scalar report for one bounded maintenance step; no free is needed.
#[derive(Clone, Copy, Debug)]
#[repr(C)]
pub struct ZeGraphMaintainReport {
    /// Exact sizeof this descriptor, initialized by the caller.
    pub abi_size: u32,
    /// 1 when the cycle finishes; 0 when another bounded step is needed.
    pub cycle_complete: u32,
    /// Last published generation.
    pub generation: u64,
    /// Physical references replaced.
    pub replaced_physical_refs: u64,
    /// Artifact bytes written.
    pub new_pack_bytes: u64,
    /// Live bytes copied from selected packs.
    pub relocated_bytes: u64,
    /// Packs selected for draining.
    pub drained_packs: u64,
    /// Bytes covered by reclamation.
    pub reclaimed_bytes: u64,
    /// Bytes actually removed.
    pub removed_bytes: u64,
}

#[path = "graph_entry.rs"]
mod graph_entry;
pub use graph_entry::*;
