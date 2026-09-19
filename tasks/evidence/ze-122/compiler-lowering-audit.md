# ZE-122: Cypher read-lowering readiness audit

Read-only delegated audit, 2026-09-19. No tracker, specification, product or
existing worktree edits; no build or test was run for this audit. Recommendations
below are not claims that proposed interfaces have already been implemented.

## Decision

**Production read lowering can be implemented before ZE-50/51 physical
operators, but the complete accepted read profile cannot yet target the current
IR without a small pattern-contract addition.** ZE-55 already preserves the
required source/semantic information. Waiting for storage scans, aggregation
execution and completed results is unnecessary for constructing, charging,
validating and source-mapping a faithful plan. Those producers remain mandatory
for original ZE-56 public-execution and TCK acceptance.

Do not simply remove ZE-51 from original ZE-56. Keep ZE-56 as the real-producer
read integration/acceptance ticket, with all requirements below. Create one flat
production-lowering implementation ticket after ZE-55 and the reviewed typed
pattern contract are committed. This new ticket implements the complete
accepted read lowering, not a scalar-only demonstration. Original ZE-56 must
depend on that implementation, real ZE-50/51 execution, and the real public
GraphStore/completed-result producer in ZE-53 (or their explicitly tracked
integration successors if those tickets are also split). Keep ZE-59/release
transitively blocked on this real acceptance. Check the resulting dependency
graph for cycles before updating it.

## Authoritative inspected versions

- Current main at audit start:
  `8517c9e5f2f75b6d35ca1cffbf96c5bcbed54978`.
- Final ZE-55 candidate:
  `5c9e757c05f977aa864a82fb3ab40630aff976b8`.
- ZE-120 evidence-only review:
  `8d6a92731eaf86d925017f6ee48f6d2e0805e1e8`.
- Live ZE-56 was `todo`, blocked by ZE-29, ZE-51, ZE-55; ZE-50 was blocked
  by ZE-29, ZE-45, ZE-49; ZE-51 by ZE-29, ZE-50. ZE-122 authorizes revising
  implementation readiness while preserving integration acceptance.
- Accepted `docs/graph/plans/cypher.md` SHA-256:
  `9aa1af2a190012a32ac6b671ece51624191e9ecf4eac4904e85ff6b98c8f978c`.
- Accepted `docs/graph/plans/execution.md` SHA-256:
  `6bf49d9c0bd3093f06967d1746a19d49e5e2519e51ac184d748627daf1a5ea06`.

The immutable revisions above, rather than any moving working tree, identify
the source behind this assessment. ZE-55 source has already passed the separate
ZE-120 audit; that is binding/resource evidence, not read execution evidence.

## Existing input contracts that are sufficient

Paths in this section are relative to `crates/zeppelin-embed-cypher/` at the
final ZE-55 candidate unless otherwise qualified.

| Source/signature | Stable information and ownership |
|---|---|
| `src/binding.rs::compile_with<T>(text: &str, parameters: &[ParameterBinding<'_>], limits: CompileLimits, resources: &mut dyn Resources, consume: impl for<'query> FnOnce(BoundQuery<'query>) -> Result<T, ParseError>) -> Result<T, ParseError>` | Complete parse/bind precedes callback entry. The callback cannot return frontend borrows. No writer, catalog or GraphReadView admission occurs. |
| `src/shared_resources.rs::compile_in<T>(text, parameters, limits, memory: &QueryMemory<'_>, control: &QueryControl, consume: impl for<'query> FnOnce(BoundQuery<'query>) -> Result<T, ParseError>)` | Existing shared query/store charge, compiler scratch, actual allocation capacities and growth overlap; caller source/parameter backing is borrowed synchronously and is not an actual-owner credit. |
| `BoundQuery::syntax() -> &Ast`, `Ast::{root,node,nodes,text,source}`, each `Node::{kind,span,children()}` | Exact source-ordered clauses, optional flag and attached predicate, node label conjunctions, relationship OR names/direction/finite bounds, inline property names/expressions, WHERE/WITH/RETURN, aggregate/distinct/order/skip/limit, YIELD and source spans. There is no syntax erasure requiring a second parser. |
| `BoundQuery::fact(AstId) -> Option<BoundFact>` | Possible kinds/nullability, source slot where applicable, row dependence, entity origin and singleton-distinct-node eligibility provenance. Pattern nodes include anonymous slots; mutation/YIELD nodes have binding facts. |
| `BoundQuery::expressions() -> &[Expression<'_>]`, `parameters() -> &[ParameterBinding<'_>]` | Existing typed core scalar/aggregate operations, literal bits, symbolic property/label names, full-width ID and StoredText operations. Expression IDs are AST-indexed with non-expression holes and possible synthesized trailing expressions. |
| `BoundQuery::projections() -> &[BoundProjection<'_>]`, `BoundProjection::{syntax,columns}` and `BoundColumn::{name,kinds,expression,slot}` | Every projection's ordered exact output scope, expression root and destination SlotId, including wildcard expansion. Lowering can retain hidden input/order expressions only at the permitted projection stage, then remove them from the exposed scope. |
| `BoundQuery::calls() -> &[BoundCall]` | Exact source call ID/procedure plus distinct Default/Auto/Exact/Scan and AllIndexed/Empty/GlobalDistinctNodes provenance. AST retains arguments/YIELD names and nullable component yields. This is the later ZE-58 handoff, not implemented search lowering. |
| `BoundQuery::requires_deleted_runtime_validation()` | Later write lowerer must retain the explicit dynamic validation obligation. ZE-56 cannot discard it while composing shared read projections; ZE-57 owns the runtime enforcement. |

All relevant fields/accessors already exist. A new lowerer in this same internal
crate can use crate-private AstId indices; no public prepared-query API or new
third-party dependency is needed. Do not treat `BoundFact` possible kinds as
proof that an actual nullable value is always invalid or always valid: ZE-120
already corrected that mistake for property lists. Preserve the corrected
ORDER BY output-alias priority.

## Existing typed output/resource contracts

Paths here are below `crates/zeppelin-embed/src/property_graph/query/` at main
8517c9e unless the ZE-55 addition is explicitly named.

- `plan/mod.rs`: distinct `PlanNodeId`, `ExprId`, `SlotId`, `PatternId`,
  `ParameterId`; `PlanDescription<'a> { operators, expressions, parameters,
  root, eager_searches }`; existing Unit, ScanNodes, Lookup*, Expand,
  BoundedExpand, Join, OptionalApply, Filter, Project, With, Aggregate,
  Distinct, Sort, OffsetLimit and Collect variants. Scalar/function operators
  already cover the accepted read expression inventory.
- `GraphPlan::validate_with_fact_vec(description, &mut Vec<NodeFacts>,
  PlanFootprint, PlanBacking, &mut ValueContext) -> Result<GraphPlan, PlanError>`
  validates actual complete fact-Vec ownership; raw-slice validation alone is
  insufficient for later runtime admission. Core independently enforces scope,
  reachability, cycle, arity, depth/width, aggregate/search/mutation barriers and
  the 16-hop limit. Lowering must prune/remap syntax holes and include every
  reachable synthesized expression; it cannot pass the binder's entire sparse
  expression array as a valid core plan.
- `resources.rs::QueryMemory`, fixed `QueryArena<T>` and ZE-55's appended
  `QueryExternalReservation` provide the existing one-query/same-store account.
  Use charged actual-capacity owners for operator/edge/projection/sort/parameter/
  expression/name/span backing and fact storage; hold frontend/new-plan overlap.
  No independent 24 MiB account per stage, numeric prepayment or fabricated
  retained address capability is permitted.
- `resources/inputs.rs`: `RetainedAllocation::{vector,string,array,boxed,arena,
  plan_facts}`, `RetentionInventory::{array,vector}`, `QueryInputs::reserve`
  and `QueryInputs::admit_plan` are the actual-owner runtime route. Same-owner
  QueryArena backing is not charged twice. Borrowed caller parameters need
  complete owner proofs, including nested backing, or charged copies.
- `runtime.rs`/`runtime/driver.rs`: `RetainedView`, `RuntimeContext`, cumulative
  `RuntimeLimits`/22 `WorkKind` counters, fixed RowBatch, `PullOperator`,
  `Completion`, and `execute` are implemented foundations. `PullOperator::pull`
  is an adapter, not an implementation of scans, aggregation or every IR node.
  `FrozenOutput` metadata is not the ZE-53 copied graph representation.
  `RetainedView` cannot be replaced by a synthetic QueryView for public evidence.

The private lowerer should use a scoped consumer over its retained arena/fact
owners and validated GraphPlan, with an explicit expression/operator-to-source
span map. It should accept the current validation/control inputs (including
`&mut ValueContext`, as the validator currently requires) and the same
`QueryMemory`; it must not manufacture a retained read lease to satisfy this
signature. The compiler can construct/validate symbolic plans without a
physical operator factory. Runtime admission and public result ownership stay
separate. Freeze the exact Rust lifetime signature in compiled code, not just a
diagram or an uncompiled illustrative signature in a planning document.

## Concrete IR gaps to resolve before claiming complete read lowering

1. **Relationship type alternatives.** Both existing Expand variants have
   `relationship_type: Option<GraphName<'a>>`, while the accepted AST preserves
   zero or more OR alternatives. Fixed-hop OR could be faithfully lowered to
   a type predicate, but bounded paths require testing every traversed edge.
   Freeze one shared representation, preferably a borrowed
   `relationship_types: &'a [GraphName<'a>]` for both variants, with empty
   meaning unrestricted and nonempty meaning OR. Specify duplicate-name
   handling, symbol resolution/missing-type semantics, name-backing charge,
   cancellation and stable original endpoint identity. No truncating to the
   first type or importing a string filter.

2. **Bounded per-edge property predicates.** BoundedExpand currently has no
   predicate or scalar current-edge scope. A filter on the final relationship
   list is not equivalent and the accepted language does not provide a generic
   `all` function to repair this afterward. Add a small typed predicate record,
   e.g. a current-edge `SlotId` plus `ExprId`, whose validation scope is input
   bindings plus one nonnullable REL slot. The predicate is checked for each
   candidate traversed edge before it enters a path; null is not a match.
   The internal current-edge slot is distinct from the public relationship-list
   output and cannot leak into output facts. Zero-hop paths have no edge checks;
   absence of a property predicate remains distinct from a false predicate.
   Lower inline property equality conjunctively through existing Expression
   operations. Coordinate exact scope/name rules with ZE-50 before either side
   independently invents a variant.

3. **Specify existing correlation and reuse, then compile-check it.** Current
   OptionalApply takes two input node IDs; current tests represent the right
   branch as descending from the left input. Freeze how a physical operator
   rebinds that anchor per preserved row, keeps row multiplicities, evaluates
   the complete right-side predicate before null extension and preserves only
   previously bound slots on failure. No new OptionalApply field is demonstrably
   required for lowering if this existing DAG contract is made exact. Expand
   adds new output slots; reuse of an existing node/relationship therefore
   requires internal fresh outputs plus equality/reprojection, or a reviewed
   explicit reuse descriptor. Do not overwrite an existing slot. Freeze one
   PatternId per MATCH including comma-separated parts, a fresh one on the next
   MATCH, and retained relationship uniqueness across joins. The current
   lineage validator is structural checking, not the runtime uniqueness set.

4. **Projection-stage scope and accounting need tests, not new generic IR.**
   Existing operators can express grouped/global aggregation, DISTINCT, hidden
   order keys for ordinary projection, output-alias precedence, WITH scope,
   SKIP/LIMIT and final projection. Query-invariant I64 limits can be resolved
   from the already validated bindings into OffsetLimit's u64 fields. Freeze
   source mapping and charged owner/lifetime handling in the private lowerer.
   A scoped validator can prove the generated plan has the right facts/barriers;
   only runtime tests prove its rows and order.

5. **Search gaps are real but belong to ZE-58.** Existing `plan/search.rs`
   `SearchMode` is only Exact/Approximate, and `OperatorKind::Search` has one
   node and one score output. It cannot faithfully express all four frontend
   modes or optional hybrid `vector_distance`/`lexical_score` outputs; no typed
   eligible-set producer variant is yet exposed here either. Retain complete
   binding data. These differences do not block standalone non-search ZE-56
   lowering, but they do block claiming complete CALL composition or mapping
   Default/Auto/Scan into Approximate. Coordinate the later IR extension with
   ZE-51 eligibility and ZE-58, and with the ZE-67 C shape already retaining
   these distinctions. Do not ship a generic rejection/stub for an accepted
   CALL in a supposedly complete compiler route.

There is no mandatory new IR for conjunctive node labels: choose a ScanNodes
label and apply remaining HasLabel predicates under the same scope. Likewise
fixed relationship property constraints can use exact typed filters. These
are faithful compositions, subject to the same runtime limits and source-map
checks, rather than reasons to serialize all compiler work behind storage.

## Minimal source ownership and compile-check gate

Give one owner the typed-pattern delta in `query/plan/mod.rs`,
`plan/validate.rs`, and any required `plan/expression.rs`/`lineage.rs` handling.
Add narrow structured-plan tests in `tests/graph_query_plan.rs` for both good
and malformed alternatives/per-edge scopes, name backing, unreachable
expressions, bounds, optional anchor/reuse and cancellation. Existing C shape
tests/fixtures must be updated by their owner if the concrete mapping changes.
Land/review that contract and record its immutable commit before parallel
lowering/operator workers consume it; do not allow both to change it independently.

The new lowerer owns new files such as `zeppelin-embed-cypher/src/lowering/`
and its read-lowering tests, plus a minimal module/private entrypoint export.
Its existing inputs are `binding.rs`, `binding/{patterns,projection,expressions,
search}.rs`, `ast.rs` and `shared_resources.rs`; they should change only for a
demonstrated missing typed accessor. ZE-50/51 own physical implementations
under core `query/`, never a second parser or a compiler-side graph interpreter.

Before calling the new implementation ticket ready, require a compiled private
consumer that accepts final BoundQuery, builds a real charged PlanDescription,
validates a real fact Vec, preserves a source-span map and demonstrates that its
borrows cannot escape. Add focused RED/GREEN cases for the newly frozen pattern
fields and a complete projection/optional shape. This can run with finite
in-memory input data to test the contract; such adapters remain explicitly
non-acceptance evidence for storage, public execution or TCK.

Implementation acceptance then requires lowering every supported read form,
all exact rejection boundaries, faithful barriers/types/source spans, actual
same-owner capacity and cancellation cleanup, reachability/remap checks and
directed comparator/fault controls. It does not require waiting for ZE-50/51 to
finish executing those plans. It must not rename a partial tracer as the full
lowerer or close original ZE-56 on that basis.

## Lossless mapping of original ZE-56 requirements to mandatory later gates

| Original requirement | Implementation evidence possible now | Mandatory real-producer acceptance retained on ZE-56/integration |
|---|---|---|
| MATCH/OPTIONAL, node/relationship reuse, finite paths and relationship-list bindings | Exact operators, per-MATCH PatternId, OR types, per-edge predicates, list output kinds, optional anchor/predicate/nullability and no syntax loss | Execute through actual ZE-45 view/cursors and ZE-50 operators; exact full-ID tuples, parallel/self edges, undirected self-loop once, zero hops, repeated nodes, no repeated edge within one MATCH, subsequent MATCH reuse, disconnected patterns, whole optional failure and attached WHERE; repeat after compaction/reopen |
| WHERE/WITH/RETURN and expression functions | Expression remap, source spans, exact projected scope, static parameter/type rejection, ID/StoredText spelling maps to core operations | Actual three-valued filtering, properties/missing labels versus variables, ID strings preserving u128, absent/empty/stored text in the same view, list indexing/nulls, arithmetic/domain errors and actual output types/bags; no second admission |
| count/collect, grouping, DISTINCT | Correct grouping versus aggregate roots, equivalence semantics requested, global singleton facts | ZE-51 executes empty global 0/[] versus grouped zero rows, null skipping, DISTINCT null/NaN/exact numeric/list equivalence, bounded collection and ordered collect semantics |
| ORDER BY, SKIP/LIMIT | Proper projection stage, alias priority, hidden-key scope then pruning, checked exact nonnegative bound values and barriers | Runtime oracle compares explicit order and tied-key bags; stable requested sort, no accidental input-bag dedup; LIMIT never bypasses work/memory/source obligations or preceding mutation effects when composed under ZE-57 |
| Original selected positive and expected-error TCK coordinates | Original-byte fixture inventory and per-statement binding/lowering states; plan validation does not update execution state | Execute all 57 selected read scenarios listed below, original setup/query/parameters/results/error phase intact; compare types/nulls/bags/order and unchanged graph state via the public Rust compiler-to-GraphStore path using real producers/completed results |
| Independent tiny-graph oracle | Independent plan-shape/binding comparator and deliberate failure can expose compiler omissions | Independent evaluator, not product decoder/planner/scalar implementation, compares actual result values/tuples/bags/order against tiny persisted graphs and legal alternate start/join plans |
| Unbounded/named path/UNWIND and unsupported original function forms | Exact early rejection, no callback entry, no hidden fallback; maintain rejected-profile labels | Public rejection stage/category and no effects/admission after whole-statement compile rejection; finite adapted/local equivalents stay separately labeled, never original passes |
| Named RED then GREEN at relevant public compiler/core boundary | Named real compiler-to-GraphPlan failures and terminal correction; allocation/control faults at actual introduced sites | Public execution RED/GREEN for runtime/result semantics when producers exist; synthetic returned rows or interpreted plan fixtures cannot satisfy this requirement |
| Reachable cancellation/resource/adversarial checks, can-fire and same-seed clean controls | Plan-building growth, limits, parameter copying, validation/source mapping, owner release and intentional compiler comparator failures | During real scans, expansions, joins, grouping/sorting/materialization and close; real-site work/capacity counters, no partial output, complete release and no store changes; extend the actual runner's registered routes |
| Pinned source reuse, std-only outer compiler/dependency constraints and exact profile exclusions | Source/notice and Cargo audits remain directly checkable | Public/error/TCK outcomes still match the chosen profile; no upstream dialect silently adopted; final shipping/coverage/size gates remain mandatory in their owners/ZE-118 |

The 57 read scenarios are derived from the committed manifest, not an invented
smaller acceptance set: Match1[1–6], Match2[1–8], Match3[17,18,23,29],
Match4[1,3,6], Match7[1,8,10,24], MatchWhere6[2,4], With6[1–3],
Aggregation1[1,2], Aggregation5[1,2], Aggregation8[1,2], Boolean1–4[1],
Null1[1–4,6], List1[1–4], List3[1–7], Return5[2]. Match1[6] and Match2[8]
retain compile-time SyntaxError/InvalidParameterUse; Match3[29] retains
compile-time SyntaxError/RelationshipUniquenessViolation. The remaining 54
are original positive read executions. Original List1[5] (`toInteger`) remains
rejected-profile. Remove1[2,4,7] remain rejected for keys/sum and local supported
remove fixtures remain ZE-57/59 obligations, not extra ZE-56 passing rows.

ZE-59 still integrates all 99 original read/write scenarios, complete published
profile support/boundary evidence and extensions. C/Swift parity, whole-suite
adversarial/coverage/performance/shipping qualification remain later mandatory
owners/ZE-118; scheduling them later does not delete them.

## Source anchors for the contract freeze

SHA-256 of inspected committed files:

| Main 8517c9e source | SHA-256 |
|---|---|
| `query/plan/mod.rs` | `e688e528be3f7c3d2c4c5ebd08d85fd41caa2e029aa80465768a9d25163f71ca` |
| `query/plan/validate.rs` | `5296a8cd693a722bd221928c772a0e406dfc74a5ea7e221c86b874f1b09cac94` |
| `query/plan/lineage.rs` | `6c30deebbe4807a9b262ff31ce96fc16863d9a855209f2c881c7e49317b36a14` |
| `query/plan/search.rs` | `eac3af5f93ac6f00d6b600138344159dd2e9de844c680829d5e10327ca3c781b` |
| `query/resources/inputs.rs` | `36e9bf5d6b2f4e71a998f2ad043cc6b976ea70ff0278dd017e5b4a85e5c72608` |
| `query/runtime.rs` | `44efce27f3c20a148840a7b75edc61434186922e280ec1563ba37918dd390f5d` |
| `query/runtime/driver.rs` | `e0cb26386eb28c6626cb3ce89e38cd3f8d33b54582319de3e41eb52f53ecdb43` |

| Final ZE-55 source | SHA-256 |
|---|---|
| `src/binding.rs` | `759684521795237dccf7d5f932966ec8841fcb988f3700ae7bdecd32bb1df35e` |
| `src/binding/projection.rs` | `beafcd62488c59390cfbb126b78126d25f3289943240c7afecf691c5a5e99cc5` |
| `src/binding/patterns.rs` | `9dea4c9d6d560200482e2b60743348b9019b7e542778d1088117a8543663f08e` |
| `src/binding/search.rs` | `642add35510d3e71f2f9c86847f2dcde5830f86dcc2e6fed3291af6805f05c4b` |
| `src/shared_resources.rs` | `20adf570b13355291a1afde073a0044b6dfdb5466a34971081457ce9ea7e3063` |

Revalidate only changed anchors after main integration. A new source revision
or an agreed interface extension requires updating the freeze artifact and its
focused consumer tests before parallel owners rely on it.
