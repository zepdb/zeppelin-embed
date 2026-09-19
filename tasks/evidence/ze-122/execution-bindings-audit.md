# ZE-122: execution and bindings parallelization audit

Read-only audit, 2026-09-19, main
`8517c9e5f2f75b6d35ca1cffbf96c5bcbed54978`. No product/tracker changes or tests
were run. Read the complete accepted execution and bindings plans, the live
ZE-49/50/51/52/53/68/69/122 tickets, and the source inventory adjacent to this
file. `execution-bindings-tickets-before.json` preserves the original scopes;
`execution-bindings-source-inventory.json` pins inspected bytes. Existing
compiled interfaces below are landed code with their recorded focused evidence;
proposed interfaces are explicitly distinguished and have not been compiled.

## Recommendation

Keep ZE-50, ZE-51, ZE-53 and ZE-68 as mandatory real-producer integration and
acceptance tickets. Add flat implementation tickets for bounded relational
kernels/eligibility, owned completed-value storage, and aligned C result
ownership. These components can run in parallel with storage, pattern
execution and publication once their small contracts below are recorded.
Do not unblock the original tickets by merely deleting their upstream edges.

The strongest immediately implementable component is eligibility plus the
relational value kernels: landed query values, exact equivalence/hash/order,
fixed charged arenas, controls and work counters already exist. It does not
need a graph scan implementation. The C arena/registry owner is also independent
of a completed core producer; its tests can exercise real allocations and real
registration/free without claiming any graph query or store-lifetime proof.
Native completed-value storage needs the explicit owned-representation and
detachment contract below before two implementations rely on it. Its query
and publication adapters remain with ZE-53/68.

## Landed seams and precise limitations

Paths in this section are relative to the repository root.

| Existing interface | Source and exact boundary | What it proves / does not prove |
|---|---|---|
| `RetainedView` | `query/runtime.rs:16`: `query_view(&self) -> &QueryView`; `check_active(&self) -> Result<(), QueryError>` | Mandatory stable token and retained lifecycle check. It is an adapter, not an implementation of native graph admission. |
| `RuntimeContext<'v,'m,'g>` | `query/runtime.rs:194`: `new(&'v dyn RetainedView, &'v QueryControl, &'m QueryMemory<'g>, RuntimeLimits) -> Result<Self, RuntimeError>`; `checkpoint`, `check_work`, `charge`, `memory`, `values`, `view` | Same close-first control, query owner and cumulative 22 work categories. No graph I/O or durable publication. |
| `PullOperator` | `query/runtime/driver.rs:23`: `node() -> PlanNodeId`; `prepare_search(&mut self, PlanNodeId, &mut RuntimeContext<'_, '_, '_>) -> Result<(), RuntimeError>`; `pull<'v,'m,'g>(&mut self, &mut RuntimeContext<'v,'m,'g>, &mut RowBatch<'v,'m,'g>) -> Result<PullState, RuntimeError>` | A bounded root producer. Eager obligations precede pulls. It is not yet a physical plan tree, storage source, expression evaluator, or generic graph factory. |
| `RowBatch<'v,'m,'g>` | `query/runtime/batch.rs:294`; `with_arenas(&RuntimeContext<'v,'m,'g>, columns, max_rows, payload_limit, ArenaCapacity) -> Result<Self, RuntimeError>`; `push_row(&[QueryValue<'_>], &mut RuntimeContext<'v,'m,'g>)`; `value(row,column) -> Option<QueryValue<'_>>` | Copies scalars/strings/lists/full IDs into actual charged backing. Public scheduling constructor limits rows to 1..=256. No column-to-`SlotId` schema is stored in this type. |
| `PreparedRows` / `Completion` | `query/runtime/driver.rs:70,94`: `complete<'v>(&mut self, &PreparedRows<'v,'m,'g>, &mut RuntimeContext<'v,'m,'g>) -> Result<FrozenOutput<Self::Output>, RuntimeError>` | Completion cannot return a borrow of the fresh view/rows lifetime. Associated output may still retain `'m/'g` query accounting. Entity copying is not implemented. |
| `FrozenOutput<T>` | `driver.rs:108`: `new(output, rows, core_bytes, abi_bytes)` | Checks represented rows <=65,536, core/ABI bytes each <=4 MiB. Caller-supplied lengths are not evidence of actual buffer capacity, registry ownership or entity correctness. |
| Runtime drain | `driver.rs:181`: `execute<'m,'g,V:RetainedView,O:PullOperator,C:Completion<'m,'g>>(view:V, control:&QueryControl, memory:&'m QueryMemory<'g>, plan:&RuntimePlan<...>, source:&mut O, completion:&mut C, capacity:ExecutionCapacity, limits:RuntimeLimits) -> Result<Execution<C::Output>,RuntimeFailure>` | Eager sources once; private rows; final close/cancel check; no partial rows on failure. This is a read/preparation driver with no irreversible commit window. Do not call its last cancellation check after a durable write. |
| `QueryMemory` | `query/resources.rs:44`: `new(&GraphResources, limit)`; `adopt_shared(GraphReservation) -> Result<QuerySharedReservation, (MemoryError, GraphReservation)>` | <=24 MiB query budget inside actual store accounting. Adoption consumes a same-store charge, adds local/control charge, returns authentic original charge on failure. No numeric prepayment substitute. |
| `QueryArena<'m,'g,T>` | `query/resources.rs:199`: `new(&'m QueryMemory<'g>, capacity)`, fixed `push`, slices | Reserves before allocation and reconciles actual Vec capacity, including unused capacity. No public owned-result detachment or implicit growth. Contains a query-lifetime reservation. |
| Owned input proof | `query/resources/inputs.rs:12,103,319`: `RetainedAllocation::{vector,string,array,boxed,arena,plan_facts}`, `QueryInputs::reserve/admit_plan`, `RuntimePlan` | Whole owner/capacity evidence retained with backing, not a slice-length assertion. Runtime plans require real facts Vec ownership. |
| Value kernels | `query/grouping.rs:9,54,63`: `QueryValue::{order,equivalent,group_hash}(...,&mut ValueContext)`; `query/value.rs`, `scalar.rs`, `list.rs` | Exact numeric/null/NaN/list/entity semantics already available, under controls. Hash collisions require equivalence checks. These are not operators. |
| Plan scopes | `query/plan/mod.rs:559`: `NodeFacts::{width,slot,ordered,singleton,classification,barriers}` | Validated facts exist. There is no public ordinal slot iterator/mapping; do not treat logical slot ID as a cell index. |
| Write result preparation | `staging/result.rs:5,18,25`: `ResultLayout {rows,core_bytes,abi_bytes,registry_bytes}`; `ResultRegistration::capacity_bytes`; `ResultMaterializer::layout(receipt_count,&mut WriteControl)`, `materialize(&[ItemReceipt],&mut [u8],&mut [u8],&mut WriteControl) -> Registration` | Complete structured receipt copies and a real token before publication handoff. It does not accept `PreparedRows` or general completed query values. |
| Result adoption | `staging/result.rs:203`: `MaterializedBatch::adopt_result_memory(self, base, adopt(GraphReservation)->Result<Q,(StageError,GraphReservation)>, control)` | Moves actual core/ABI/registry charges into joint query ownership while retaining writer guards. Still private, borrowed writer/query lifetime; not application-owned result exposure. |
| C shape | `ffi/src/graph_contracts.rs`: 40 structs, 19 enums, 141 discriminants; `ZeGraphResponse` and typed value pools | ZE-67 fixes representation, fields, flags, bounded ranges, strong IDs and errors. No runtime marshalling or ownership is implemented. |
| Existing allocation registry | `ffi/src/registry.rs:42,55,59,335,350,366,396`: pointer + element TypeId + generation + lengths | Authentic legacy validation/free exists. `HashMap::insert` in `register_arena` can allocate. There is no charged reserve/publish token or frozen graph-root descriptor validation. Reusing it unchanged cannot prove no postcommit allocation. |

All `query/...` paths above begin
`crates/zeppelin-embed/src/property_graph/`; all `ffi/...` paths begin
`crates/zeppelin-embed-ffi/`.

Absent from this main: `GraphStore`, native `GraphReadView`, `EligibleNodeSet`,
`CompletedGraphResult`, graph expression evaluation, physical pattern/relational
operators, `GraphQueryError`, graph registered result arenas and graph exports.
The docs' names are proposed contracts, not callable product APIs. Also, current
core IR lacks the full accepted OR-type/per-edge predicate/eligible-set/search
tier/hybrid-component representations already preserved by the C schema. Their
owners must extend the IR; a new component must not freeze that omission.

## Minimal contract freeze: relational and eligibility implementation

Freeze these invariants under ZE-122 and assign one owner for the small shared
runtime additions. None requires changing accepted semantics.

1. A row schema owns an ordered list of distinct `SlotId`s and resolves logical
   slots by checked lookup. Add a read-only `NodeFacts` ordinal iterator/accessor
   or construct the schema once from validated facts through an internal API.
   Operators never infer `column = slot.0`. Charge schema and operator state.
2. Kernels consume existing `QueryValue` and row batches under the same
   `RuntimeContext`; no new budget or view. A bounded row store for blocking
   aggregate/distinct/sort is a distinct charged owner, not a public scheduling
   `RowBatch` with its 256-row cap silently lifted. Its backing stays accounted
   while output batches are copied. Capacities and overlap are explicit.
3. Keep expression evaluation as a mandatory internal typed producer. The
   relational ticket implements all scalar/value operations and the relational
   algorithms it owns; property/label/type/stored-text reads use the later
   authentic base/overlay accessor. Do not ship an all-null/default adapter or
   translate missing accessors to success. If a callback seam is used for kernel
   tests, label that evidence as kernel semantics only. Bind it to an expression
   scratch owner or copied output; never return an uncharged temporary string.
4. Existing `RuntimeError` has only Limit/Value/Memory/Batch. It cannot faithfully
   report future storage corruption/I/O or `StageError::DeletedEntity`. Freeze
   typed additional causes with the storage/execution owner before connecting
   entity reads; do not collapse these into Batch, Internal or Cancelled.
5. Suggested new opaque type (not yet compiled):
   `EligibleNodeSet<'v,'m,'g> { view: &'v QueryView, ids: QueryArena<'m,'g,NodeId> }`.
   Private fields; construction takes a same-context validated node list,
   checks every member including nested/type errors, validates token identity,
   examines at most 524,288 IDs, and produces sorted unique full IDs. A borrowed
   capability exposes those IDs and the identical view token to retrieval.
   `AllIndexed` is a separate enum arm, never an empty set convention.
6. Same-view set construction charges one actual packed-ID owner. If source
   storage is still retained, charge the copy and sorting scratch simultaneously;
   consume a dead buffer only through an authentic consuming owner API.
   Use a cancellable in-place algorithm or explicitly charged bounded scratch;
   an uninterruptible standard sort is insufficient. Dedup never changes input
   row bags. Eligibility cardinality, examined entries, sorting/hashing and
   copied bytes are actual counters, not lengths of requested output.
7. Check the composed producer API with a concrete compile/test example before
   freezing a factory: `PullOperator::pull` is generic over fresh context/view
   lifetimes, whereas retained `RowBatch` state binds fixed lifetimes. Do not
   assume a chain of buffer-owning operators composes merely because a single
   root adapter compiled in ZE-49. A factory may bind one execution's lifetimes;
   no unsafe lifetime widening or public mutable query context is acceptable.

Independent implementation acceptance can use real fixed arenas and independently
computed input/output bags. It must cover empty/global/grouped aggregates,
null-skipping and DISTINCT semantics, bounded stable sorting, rows spanning many
batches, explicit limits, every allocation/cancel failure and actual release.
Native scans, optional joins, real property reads and public admission remain
mandatory in ZE-51. A result kernel test is not that integration evidence.

## Minimal contract freeze: native completed-value owner

Add a flat implementation ticket rather than making ZE-53's complete public path
pretend ready. Freeze an internal owned result description and builder API that
both native and C consumers can use without a storage dependency.

- Owned values are explicit Null/Bool/I64/F64/String/Node/Rel/List. Strings,
  ordered list children, columns, rows, properties, labels and entities use
  bounded checked indices into owned typed arrays. Core types must not import
  the FFI crate or treat a C header as their implementation.
- Copied node/relationship records carry strong full IDs, endpoints/type,
  labels/properties, revision, optional exact key and last-change generation.
  Native source text/vector are absent by default; typed entity-get selection
  controls them. Stored-text expression yields an independent copied string.
  No recursive neighboring entities, raw read references or lazy lookup.
- A prepared result owns its real charged buffers and the complete initialized
  lengths. It may retain query-accounting lifetimes, but must not retain the
  fresh view/cursor/input lifetime. Only a successful finalization consumes it
  into `CompletedGraphResult` with **no query/store lifetime parameter**.
  Avoid a `Vec<QueryValue<'_>>` masquerading as owned output.
- The consuming transition must move existing buffers without allocation,
  unregister temporary query/writer charges exactly once, and record transfer
  to caller-retained/application accounting. A `QueryArena::into_owned`-style
  internal operation requires owned element types and cannot be a general
  arbitrary-borrow escape. Failure destroys initialized buffers before guards.
- Complete represented bytes (including descriptors) <=4 MiB and rows <=65,536
  are distinct from actual retained capacities. Enforce both representation
  and shared/query/writer allowances; core and ABI can overlap, each up to4 MiB,
  plus actual controls/registry. Count every copy and poll within bounded chunks.
- Freeze native report metadata losslessly against accepted retrieval semantics
  and the ZE-67 schema: per-call source order, generation, optional requested and
  actual tier, precision versus coverage, epochs, empty-leg reasons, effective
  alpha/policies, counters. No TCK production mutation counters. Real ranking
  remains ZE-64; typed controlled reports are valid collector-level evidence.
- Deleted entities, late copy failure, malformed pool geometry, list depth/count,
  foreign view, limit/allocation and cancellation are typed failures with no
  partial public result. Entity resolution remains a mandatory producer adapter
  bound to the admitted base or permitted property/text overlay; a fabricated
  entity provider cannot close the public-path acceptance in ZE-53.

Suggested new names such as `PreparedGraphResult`, `CompletedGraphResult` and
`ResultValueId` in this section are proposed, not existing signatures. Land the
reviewed representation/build/detach API with real owner tests before beginning
its core-to-C adapter. Do not introduce a fake `GraphStore::execute` to enable
parallel compilation. ZE-53 still owns final error assembly, cancellation/drain,
read/write/search composition and the real structured execution seam.

## Minimal contract freeze: C aligned owner and outcome handoff

The independent C implementation can own actual typed pools, a complete root
descriptor, real registry reservation and safe free now, using ZE-67's frozen
shape. It should have a private prepared state and an exposed owned state.

1. **Alignment is unresolved at the existing staging hook.** `MaterializedBatch`
   allocates `Arena<u8>`; a `Vec<u8>` contract guarantees only alignment1, even
   if this allocator normally returns aligned addresses. It cannot simply be
   cast to `ZeGraphValue`, nodes or pointer-bearing structs. Use an allocation
   whose layout guarantees all typed alignments and records the correct dealloc
   layout, or an explicitly aligned owned backing type. If padding/overallocation
   is used, reserve its complete real capacity too. Do not infer alignment from
   one allocator run or turn the ABI into a compact byte protocol.
2. Prepare the C root plus all arrays before publication, and hold the genuine
   registry slot/reservation with those buffers. Preparing may fail and must
   undo private registration. Exposing a prepared result only moves ownership
   and sets known fields; it cannot hash-insert, grow, allocate, copy, format an
   error message, invoke a fallible callback or fail registry admission.
3. Existing `ResultAllocations::register_arena` is not the needed reservation
   API. Add a concrete reserved entry/token state that records pointer, type,
   generation and the complete immutable descriptor geometry. Registry growth
   and actual backing capacity must be measured/charged. `HashMap::capacity()`
   alone is not its allocator byte size. Do not claim a logical entry count is
   `ResultRegistration::capacity_bytes` or drop an authentic shared charge.
4. Free validates token/type and every authoritative pointer/count field against
   the registered owner, then drops one bounded root-owned allocation set. An
   emptied descriptor's second free is harmless; a stale copied descriptor,
   modified pointer/count/token or cross-kind free rejects without stealing the
   real owner. No recursive traversal of host-mutable list pointers to free.
5. The independent owner can have internal Rust prepare/expose/free functions
   and tests using actual C descriptors. Real C exports/header/export-matrix
   wiring must be coordinated with ZE-69/107 and preserve the default artifact.
   Synthetic data passed into the real owner proves allocation/registry behavior,
   not core entity resolution or actual query completion.
6. Native completed-result conversion is a mandatory ZE-68 integration step
   after the native owner lands. Structured receipt conversion must consume the
   existing `ItemReceipt` producer; general query rows need the new native result
   producer, not fake receipts or arbitrary numeric `FrozenOutput` sizes.
7. Extend the preparation seam concretely for aligned backing and the eventual
   move-to-application transition, in agreement with ZE-37/39/52. Preserve the
   order already proved: layout and canonical preflight before fresh IDs, all
   copies/real registration before commit, shared adoption without duplicate
   aggregate charge, final checks before coordinator handoff.
8. An outcome cell can be implemented independently as a real state machine,
   but only the coordinator establishes NotCommitted/Committed/Indeterminate.
   Keep it outside the unwind scope. Enter Indeterminate before a potentially
   committing call; retain known Committed generation/results through caught
   unwind/delivery failure; reads retain NotApplicable. Cancellation after the
   attempt cannot restore NotCommitted or erase known success. Public fault
   injection and reopen are mandatory ZE-68/69 integration evidence.

## Flat ticket and native dependency changes

The names below are placeholders for new flat story keys; no tickets/edges were
changed by this audit. Retain the originals' complete acceptance text and link
the new implementation evidence instead of rewriting them as kernel-only work.

| New independent story | Initially blocked by | Original integration gate gains |
|---|---|---|
| `REL`: Implement bounded graph relational kernels and eligible sets | ZE-122 contract acceptance, ZE-48, ZE-49 | `block ZE-51 --by REL` |
| `VALUES`: Implement owned completed graph value storage | ZE-122 native representation/transfer freeze, ZE-32, ZE-48, ZE-49 | `block ZE-53 --by VALUES` |
| `COWNER`: Implement aligned graph response ownership and registry reservation | ZE-122 aligned-owner/reservation contract acceptance, ZE-37, ZE-49, ZE-67 | `block ZE-68 --by COWNER`; conversion may wait for VALUES while owner work proceeds |

All listed existing implementation prerequisites are done on inspected main.
No deletion is necessary from ZE-50's `{ZE-45,ZE-49}`, ZE-51's `{ZE-50}`,
ZE-53's `{ZE-51,ZE-52}` or ZE-68's `{ZE-37,ZE-53,ZE-67}` sets (all also retain
ZE-29). New stories do not depend on those original integration tickets, so
there is no cycle. Their integration evidence is required by the originals.

Preserve `ZE-52 -> ZE-51/37/39/34`, `ZE-64 -> ZE-39/50/51/53/63`,
`ZE-66 -> ZE-39/53`, and `ZE-69 -> ZE-56/57/58/66/68`. Native edges already
carry acceptance through ZE-70/71/72/74 to ZE-78, which also depends on ZE-118.
Do not let ZE-74/78 replace the owners' integration tests with one late umbrella
suite. In this notation `A -> B` means A is blocked by B.

The live producer audit also identifies two missing acceptance edges. Add
`block ZE-50 --by ZE-46`: consolidation/reclamation ZE-46 already depends on
ZE-40 recovery and ZE-45 read views, so its completion supplies the required
real compaction/reopen producer. Add `block ZE-52 --by ZE-40` for its explicit
public reopened-state acceptance; ZE-39 publication alone does not implement
recovery. Neither creates a cycle in the inspected graph. Original ZE-53 and
ZE-68 then inherit recovery and compaction transitively through ZE-51/52.

Do not add a reverse edge from ZE-68 to ZE-69, or from ZE-53 to ZE-66: those
would create cycles. ZE-68's real producer is native completed results and the
coordinator, exercised through the actual internal ABI owner/free path;
ZE-69 must subsequently prove the exported public C entrypoints. ZE-53 supplies
the actual structured execution seam which ZE-66 wraps; ZE-66 must prove its
complete Rust facade. Similarly, ZE-50/51 must use actual native sources and
operators before close, while ZE-53 owns their final public query composition.

New implementation tickets must carry their exact code/allocator/cancel/oracle
acceptance; original integration tickets must name real producers, minimum
public-path cases and the additional negative controls below. Broad nonessential
campaigns remain in ZE-118 with exact changed commits/commands. Directed can-fire
and same-seed control at newly introduced failure/order seams remain local gates.

## Full preservation of original acceptance

| Original requirement | Independent implementation proof allowed | Mandatory real-producer acceptance owner |
|---|---|---|
| ZE-49 actual row/byte/work counters, oversize cell, exhaustion, before/within-batch cancel, allocation failure, zero partial rows, release | Reuse landed ZE-49; add changed-path directed tests in REL/VALUES/COWNER | ZE-50/51/53 for their operators; ZE-68 for C construction. Do not reopen/narrow closed ZE-49; do not call its adapter test native admission. |
| ZE-49 close-drain / one view / owner-derived plan backing | Existing retained adapter tests establish foundation only | ZE-45 real lease, ZE-53 real query close-drain; ZE-66/69 public wrappers |
| ZE-50 key/label/selective sources, directional and bounded traversal, full stored endpoints | Root's separately audited pattern implementation can use explicit storage contract | ZE-50 with real ZE-45 cursors and catalog, not fixture-only sources |
| ZE-50 parallel/self edges; undirected self-loop once; zero hops; variable relationship lists; repeated nodes, no repeated edge per MATCH; later-MATCH reuse | Independent tiny-graph oracle and exact tuples | ZE-50 repeats complete matrix with native storage |
| ZE-50 attached optional WHERE, disconnected patterns, hash/nested joins, preserved bags, legal starts/join permutations/barriers | Bounded operator/kernel oracle | ZE-50 exact tuples, then ZE-51 full operator composition |
| ZE-50 compaction/reopen; both endpoints live under ZE-109 | No kernel substitute | ZE-50 after real publication/recovery/maintenance dependencies; add native dependencies if existing ZE-45 path does not transitively provide needed producers |
| ZE-50 StoredText: absent/null, present-empty, zero-term text; same-view base/overlay; no extra admission/vector function | Copy/value kernel tests | ZE-50 real reads; ZE-52 real staged property/text; ZE-53 close-independent copy |
| ZE-50 concurrent publication/compaction and text usable after close | No adapter substitute | ZE-50/53 actual coordinator/view/maintenance; explicitly add required producer edges rather than silently defer to fixtures |
| ZE-51 empty global aggregate =0/[], grouped empty =no row; count/collect omit null | REL real aggregate implementation with independent expected values | ZE-51 real pattern/evaluator input and public structured query through ZE-53 |
| ZE-51 DISTINCT null/NaN/numeric/list equivalence, collision checking | REL real grouping/hash implementation + negative controls | ZE-51 real plan and scopes |
| ZE-51 scoped WITH errors, filter truth/type behavior, projection bag retention | REL row schema/value kernels | ZE-51 full evaluator/operator scope and error propagation |
| ZE-51 stable explicit sort keys, tied-order bags, upstream ordered collect, offset/limit | REL bounded cancellable sort/aggregation | ZE-51 real plans; ZE-52 mutation input order and LIMIT side effects remain intact |
| ZE-51 empty eligible set distinct from omitted, <=524288 full-ID dedup, same-view validation, unchanged outer bags | REL real packed owner and controls | ZE-51 construction in real plan; ZE-64 retrieval borrows same capability and preserves eager reports; ZE-58 Cypher lowering |
| ZE-51 LIMIT never replaces work budget; eager CALLs under LIMIT0/empty input | REL exhausted counters and owner cleanup | ZE-51/53 execution barrier; ZE-64 actual ranking; ZE-52 write barrier |
| ZE-53 copied scalar/entity/list values, complete counters/reports/error assembly, structured GraphStore seam | VALUES owned representation/collector; controlled typed reports | ZE-53 authentic ZE-50/51/52 producers; ZE-66 facade; ZE-64 actual ranking |
| ZE-53 zero partial rows on final-copy failure | VALUES fail every real allocation/copy branch | ZE-53 public read/write failure; ZE-52 mutation state unchanged before commit |
| ZE-53 result reads after close, no view/cursor/caller parameter, paired release loops | VALUES no-borrow type and actual allocator loops | ZE-53 real store close/reopen/relocation plus caller-buffer drop; ZE-68 C equivalent |
| ZE-53 close cancellation drains live work | Kernel cancellation is only component proof | ZE-53 actual admitted reader and Store close |
| ZE-53 independent calls preserve Cartesian bags; each report survives dropping scores; approximate metadata not upgraded; empty eligibility | VALUES report-retention and controlled-adapter execution allowed by original ticket | ZE-53 complete execution with typed controllable adapter; ZE-64 real ranking and same generation, retained transitively by ZE-58/65/74 |
| ZE-68 aligned typed pools, root registration, matching free, no recursive unbounded free | COWNER actual allocator and real registry owner | ZE-68 conversion from authentic VALUES/ZE-53 completed results; ZE-69 public calls/free |
| ZE-68 heap-flat graph/list/entity/string loops; empty/second free; stale copied descriptor/type/token/pointer/count rejection | COWNER real owned arenas and hostile-copy tests | ZE-68 repeated genuine completed-result conversion; ZE-69 public C path and legacy cross-kind isolation |
| ZE-68 no retained graph lease, no corruption after close/reopen | Owning type/pointer-shape proofs only | ZE-68/69 real store close/reopen plus outstanding C result |
| ZE-68 core-to-ABI peak overlap, real registry capacity reserved before commit | COWNER/VALUES allocator failures, overlap counter exactness | ZE-68 with ZE-37/52/39 preparation/publication; force allocation denied throughout actual commit/delivery window |
| ZE-68 known outcome retained outside unwind; conservative Indeterminate; caught panic cannot claim NoEffects | COWNER outcome-state transitions only | ZE-68/69 real coordinator/public faults; known generation retained; close/reopen verifies durable state |
| ZE-52 all original repeated-target/item-order/alias/permissible-outcome/late-error/RETURN LIMIT0 criteria | No change to scope or substitute proof | ZE-52 remains blocked on real ZE-39 and ZE-51; ZE-53/68 cannot close around this |
| All original seeded runner/can-fire/clean-control/independent comparator obligations | Each new algorithm adds its own actual failure/ordering sites, scoped campaigns | Original integrations exercise producer seams; broad campaigns, >=90% per-crate, full workspace/size/platform evidence remain ZE-118 and owning release tickets |

Tracker consistency correction: ZE-51 retains a historical ZE-102 paragraph
requiring baseline/10x hub DETACH rejection at 16,384 affected entities. That
was superseded by ZE-109's logical node-tombstone DETACH contract. Preserve the
current writes/execution plan and ZE-109 behavior, not that obsolete rejection,
when recording the mapping. This is reconciliation of an existing decision,
not authority for a new limitation.

These producer dependencies are real acceptance requirements, not reasons to
idle the independent kernels. Root should apply the reviewed native edges and
the original-to-new acceptance mapping before closing ZE-122.
