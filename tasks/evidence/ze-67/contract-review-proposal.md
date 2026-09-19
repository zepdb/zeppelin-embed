# ZE-67 C contract proposal

Base: f9d81e0806ca68c876a26ed1983a3fffaf72b902. This ticket freezes data
representations, discriminants, errors and generated declarations. ZE-68 owns
actual response ownership; ZE-69 owns pointer marshalling/runtime exports.
ZE-107 owns complete feature forwarding, packaging, exports and footprint.

## Artifact boundary

Add default-disabled FFI feature `graph-cypher`, initially exposing contract
Rust types only and failing selection outside macOS/aarch64. Do not imply that
this alone enables a working graph engine/compiler facade. Do not add runtime
stubs or declare unavailable graph functions. Add separately generated
`include/zeppelin_graph_contracts.h` from a graph-only Rust source entry and a
separate cbindgen config. It includes the existing `zeppelin_embed.h`, is not
included by that legacy header, and has its own guard. Existing legacy header
and release trim/symbol checks remain unchanged except append-only generic
error enumerators. A C consumer must intentionally include the contract header.
ZE-107 later assembles the graph artifact header and enables actual exports;
it must not put this extra header into legacy SDK/package inventories. There
are no new unmangled functions in ZE-67, so symbols.allowlist stays unchanged.

## Representations

All expandable descriptors start `abi_size:u32, abi_reserved:u32`; exact v1 size
and zero reserved/unused fields are required. Simple fixed ID/range/byte-span
primitives are explicitly frozen, not extensible. No packed structs, pointer
serialization, Rust enum-valued input fields, public view pointers, or opaque
query bytecode. Every input tag is a u32 and unknown values reject before use.
Every array uses a typed pointer and count. Numeric offsets/counts in the value
and plan pools are u32 ranges (the accepted caps are far below 2^32), checked
against actual array lengths by ZE-69 before indexing. Pointer/count safety
continues to require honest accessible caller backing.

- Separate `ZeNodeId` / `ZeRelId`: high:u64 then low:u64; `ZeGraphHandle` is a
  token:u64 wrapper. Zero IDs reject; no implicit cross-kind conversions.
- `ZeGraphRange` is start:u32,count:u32. `ZeGraphBytes` is pointer plus usize
  count. Range units are fixed by each named field, never implicit bytes.
- `ZeGraphValue` has tag, list element tag, bool, entity descriptor index,
  signed i64, f64 and range fields. Value tags 0..7 are Null, Bool, I64, F64,
  String, Node, Relationship, List. List element tags 0..5 are query/mixed,
  Bool, I64, F64, String, EmptyList. EmptyList requires count zero; typed empty
  lists preserve their tags. F64 carries all IEEE bits. Inactive fields zero.
- Versioned value pool carries actual value/child-index/string-byte/node/
  relationship/property/label/vector arrays. Node and relationship descriptors
  copy their strong full IDs, optional key, revision/last-change generation,
  labels/type/properties and fixed endpoints. Optional source text and vector
  selection use explicit presence flags independent of lengths.
- Structured batch mutations carry kind/key/revision, explicit
  create/put/delete/recreate tag and expected ID/deletion revision, delete mode,
  optional image index and separately tagged existing-node/local-operation
  endpoint references. This does not authorize caller-selected fresh IDs.
- Typed plan has separate operator, expression, input-edge, expression-child,
  projection, sort-key, mutation and parameter-declaration arrays plus value
  pools, root index and eager-search source-order indices. Operator/expression
  descriptors have named fixed-arity fields and tagged operation tables that
  cover the accepted language profile, including OR relationship types and
  per-edge bounded-path predicates; irrelevant fields must be zero. Search request
  and options are separate versioned descriptors; absent tier differs from
  explicit Auto/Exact/Scan; no textual Approximate mode is accepted. Hybrid
  component yields have independent nullable output slots. Eligibility is a
  query-local typed set slot, never raw caller IDs; presence remains separate
  from the eventual set cardinality (ZE-51 owns same-view admission).
- Open/create/read-only-open, batch, structured query, Cypher and separate
  node/relationship get requests are size-versioned. Graph open has Full
  durability fixed by contract; no legacy durability/commit-tier switch. The
  optional embedding declaration uses only the existing ZeEmbeddingTower document declaration by pointer;
  query/alignment metadata belongs to query options and never changes stored
  interpretation compatibility;
  explicit tokenizer profile remains independent of embedding absence.
- Response carries root registry token, admitted/changed generation presence,
  NotApplicable=0 / NotCommitted=1 / Committed=2 / Replayed=3 / NoOp=4 /
  Indeterminate=5, row/column/root-cell arrays, the complete value pool,
  per-operation receipts, per-call search reports and bounded owned diagnostic
  descriptors. Diagnostics contain code, optional operator/source span and
  message byte range. The arena/free/guard implementation remains ZE-68/69.
- Search reports preserve call ID, generation, requested/actual tier, selected
  epochs, score precision vs candidate coverage, component/empty-leg state,
  effective weight, policy versions and actual work. Work counters use tagged
  versioned rows mapped to ZE-49's 22 categories plus peak charged capacity;
  no production TCK side-effect counters are introduced.

## Append-only error proposal

Keep 0..34 and all symbolic names unchanged. Append generic distinctions so
legacy error consumers can recognize them without graph declarations:
35 StoreKind, 36 FormatVersion, 37 QuerySyntax, 38 QueryUnsupported,
39 Parameter, 40 Type, 41 Scope, 42 KeyConflict, 43 IncarnationConflict,
44 DeletionRevisionConflict, 45 Endpoint, 46 DeletedEntity,
47 ArithmeticDomain, 48 ArithmeticOverflow, 49 DivisionByZero,
50 IndeterminateCommit, 51 RevisionOverflow, 52 GenerationOverflow,
53 DuplicateTarget, 54 IdentityOverflow. Reuse established InvalidArgument,
StaleRevision, Unsupported, Corrupt, NotFound, Cancelled, Timeout,
BudgetExceeded, OutOfMemory and lifecycle codes when meanings match.
Regenerate cbindgen and Swift error enum from their existing source/generator.
Explicit mappings from already implemented QueryError/PlanError/KeyLifecycleError
are pure Rust contract helpers; future coordinator errors are not fabricated.

## Focused proof seams

The existing public Rust ABI layouts/generated C header/error-name function,
new graph descriptor shape validators and real C compiler consumer are the
agreed ticket seams. Literal RED then GREEN for missing strong IDs/header,
full-width same-low/different-high roundtrip, zero/cross-kind rejection, typed
empty list vs nonempty EmptyList, tags/reserved/version/checked range rejection,
representative typed plan/batch/result C construction, exact cbindgen output,
and appended errors/Swift generation. Pin every new public struct's independent
size/offset/alignment golden plus old layout tests unchanged. Deliberately
perturb layout/tag/header/error mapping individually and observe intended
failure, restore exact bytes. No unimplemented execution/ownership/fault-path
claim. No graph operation ordering is added here; broad qualification remains
ZE-118, while ZE-68/69 add actual ownership/admission fault runner coverage.
