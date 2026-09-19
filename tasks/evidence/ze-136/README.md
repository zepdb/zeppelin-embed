# ZE-136: Cypher search-lowering readiness

## Verdict

**Proceed with one production component that owns the typed search-plan delta and
scoped Cypher `CALL` lowering together.** The compiler, complete non-search
lowerer, plan validator, eager-source inventory, same-query resource owner,
eligible-set owner and completed report owner are compiled contracts. Missing
`CALL` construction is missing implementation, not an unavailable upstream
contract. The component can produce a complete validated `GraphPlan`; it cannot
claim ranking or public execution.

Current core search IR is not faithful enough to consume the accepted bound
contract unchanged. It collapses request intent to `Exact | Approximate`, emits
only `node` plus one nonnull score, and has no hybrid component slots. Those are
representation gaps, so the typed IR and compiler must have one owner and land
atomically before ZE-64 consumes them.

This is the only current contract contradiction found: accepted request/output
semantics exceed the compiled core representation. The delta below resolves it
without changing the accepted text profile. No missing storage or ranking
implementation prevents this compiler component from starting.

Audit base: `1209c45bbf5f61905c616a97113b00c54b3b62e8`. The source audit read the
canonical current-main plans and the compiled source at that commit. It did not
run product tests or alter production code.

## What is already compiled

- The binder retains source-ordered `SearchCallId`, all four frontend modes,
  omitted/empty/global-distinct eligibility, typed YIELD facts and the eight-call
  bound. Hybrid component facts are already `F64 | Null`.
  ([binding/search.rs:4-24](../../../crates/zeppelin-embed-cypher/src/binding/search.rs#L4-L24),
  [binding/search.rs:77-123](../../../crates/zeppelin-embed-cypher/src/binding/search.rs#L77-L123))
- Singleton global eligibility is real provenance: only
  `collect(DISTINCT node)` sets it, grouped output clears it, and singleton alias
  projections preserve it. It is not inferred from a list's ordinary type.
  ([binding/expressions.rs:347-376](../../../crates/zeppelin-embed-cypher/src/binding/expressions.rs#L347-L376),
  [binding/projection.rs:79-97](../../../crates/zeppelin-embed-cypher/src/binding/projection.rs#L79-L97))
- ZE-126's `compile_read_in` already copies reachable expressions, parameters,
  source maps, operators and facts into the caller's `QueryMemory` and seals them
  behind a nonescaping HRTB consumer. Its only search gap is the deliberate
  `CALL` refusal and empty eager inventory.
  ([lowering/mod.rs:27-73](../../../crates/zeppelin-embed-cypher/src/lowering/mod.rs#L27-L73),
  [lowering/mod.rs:97-159](../../../crates/zeppelin-embed-cypher/src/lowering/mod.rs#L97-L159),
  [lowering/mod.rs:529-585](../../../crates/zeppelin-embed-cypher/src/lowering/mod.rs#L529-L585))
- Core already validates one singleton input per `Search`, source-order call
  inventory, a maximum of eight calls, request expression types and literal `k`.
  A `Join` combines disjoint inputs as a Cartesian product, while the runtime
  prepares every eager source in inventory order before pulling the root.
  ([plan/search.rs:3-77](../../../crates/zeppelin-embed/src/property_graph/query/plan/search.rs#L3-L77),
  [plan/mod.rs:263-304](../../../crates/zeppelin-embed/src/property_graph/query/plan/mod.rs#L263-L304),
  [plan/validate.rs:463-486](../../../crates/zeppelin-embed/src/property_graph/query/plan/validate.rs#L463-L486),
  [runtime/driver.rs:327-364](../../../crates/zeppelin-embed/src/property_graph/query/runtime/driver.rs#L327-L364))
- Execution already owns a same-view, full-u128, sorted/deduplicated
  `EligibleNodeSet`; `AllIndexed` and an explicitly empty set are distinct.
  ([eligibility.rs:7-30](../../../crates/zeppelin-embed/src/property_graph/query/eligibility.rs#L7-L30),
  [eligibility.rs:50-110](../../../crates/zeppelin-embed/src/property_graph/query/eligibility.rs#L50-L110))
- Completed results already own one source-ordered report per invocation, even
  with zero rows or projected-away scores. `requested_tier`, `actual_tier`, score
  precision and `CandidateCoverage::{Exact, Approximate}` are separate facts.
  ([completed.rs:66-93](../../../crates/zeppelin-embed/src/property_graph/query/completed.rs#L66-L93),
  [completed/records.rs:95-165](../../../crates/zeppelin-embed/src/property_graph/query/completed/records.rs#L95-L165),
  [completed/validate.rs:226-276](../../../crates/zeppelin-embed/src/property_graph/query/completed/validate.rs#L226-L276))

These contracts match the plans: four request spellings, nullable absent hybrid
components, singleton eligible provenance, independent once-per-query calls and
reports that survive projection are explicit requirements.
([cypher.md:74-90](../../../docs/graph/plans/cypher.md#L74-L90),
[retrieval.md:61-78](../../../docs/graph/plans/retrieval.md#L61-L78))

## Exact representation delta

### Core typed plan

In `crates/zeppelin-embed/src/property_graph/query/plan/mod.rs` and
`plan/search.rs`, make request intent lossless and make every permitted YIELD
field representable:

```rust
pub enum SearchMode {
    Default, // maps later to requested_tier = None
    Auto,    // Some(SearchTier::Auto)
    Exact,   // Some(SearchTier::Exact)
    Scan,    // Some(SearchTier::Scan)
}

pub struct SearchOutputs {
    pub node: Option<SlotId>,
    pub distance: Option<SlotId>,
    pub score: Option<SlotId>,
    pub vector_distance: Option<SlotId>,
    pub lexical_score: Option<SlotId>,
}

OperatorKind::Search {
    call: SearchCallId,
    request: SearchRequest,
    outputs: SearchOutputs,
}
```

`SearchRequest::{Vector, Hybrid}` keeps `mode: SearchMode`; lexical has no mode.
The validator requires at least one output, rejects fields not supported by that
request kind, rejects duplicate/input-colliding output slots, types `node` as
`NODE`, `distance`/`score` as `F64`, and hybrid component outputs as
`F64 | NULL`. `eligible: None` remains unrestricted; `Some(expr)` remains a
materialized domain and therefore preserves literal empty versus omitted without
adding a second eligibility type.

`Approximate` must disappear from request intent. Actual completeness stays in
`SearchReport.coverage`; requested `'default'` must not become explicit Auto.
The mapping above also introduces no textual `Graph(...)` mode or tuning syntax.
The existing store tier contract independently proves why `None`, Auto, Exact
and Scan are distinct.
([lifecycle/mod.rs:1620-1667](../../../crates/zeppelin-embed/src/lifecycle/mod.rs#L1620-L1667),
[plan/search.rs:92-99](../../../crates/zeppelin-embed/src/property_graph/query/plan/search.rs#L92-L99))

### Bound compiler contract

In `crates/zeppelin-embed-cypher/src/binding/search.rs`, replace the partly
syntax-reconstructed `BoundCall` payload with a complete typed descriptor:

```rust
pub enum BoundEligibility {
    AllIndexed,
    Materialized {
        expression: ExprId,
        provenance: BoundEligibilityProvenance,
    },
}
pub enum BoundEligibilityProvenance {
    LiteralEmpty,
    GlobalDistinctNodes,
}
pub enum BoundSearchRequest {
    Vector { vector: ExprId, k: ExprId, mode: BoundSearchMode,
             eligible: BoundEligibility },
    Text { query: ExprId, k: ExprId, eligible: BoundEligibility },
    Hybrid { vector: ExprId, text: ExprId, k: ExprId,
             mode: BoundSearchMode, eligible: BoundEligibility },
}
pub struct BoundCall {
    pub syntax: AstId,
    pub id: SearchCallId,
    pub request: BoundSearchRequest,
    pub outputs: SearchOutputs,
}
```

Required query argument `ExprId`s must name their canonical bound
literal/parameter/list expression, not an `Expression::Slot` created for an
alias in the preceding row source. The binder already records canonical backing
for literals, parameters and lists in `Info.constant`, propagates it through
variable/group aliases, and copies the same `Info` into projected symbols. CALL
binding must resolve that backing before constructing `BoundSearchRequest`; if a
purported query-invariant alias has no canonical backing, it rejects rather than
emitting a foreign slot into an independent `Unit` source.
([binding/expressions.rs:52-68](../../../crates/zeppelin-embed-cypher/src/binding/expressions.rs#L52-L68),
[binding/expressions.rs:87-134](../../../crates/zeppelin-embed-cypher/src/binding/expressions.rs#L87-L134),
[binding/projection.rs:42-73](../../../crates/zeppelin-embed-cypher/src/binding/projection.rs#L42-L73))

A global eligibility expression deliberately retains the evaluated aggregate
slot in the current singleton scope. Literal `[]` retains its expression rather
than only an `Empty` tag. YIELD binding writes the allocated alias slot directly
into `SearchOutputs`; later lowering need not infer typed output identity from
AST position.

The provenance enum is compiler proof only. Core still validates the complete
plan and retrieval still validates every actual node/view while constructing
`EligibleNodeSet`; Cypher provenance should not be copied into runtime set
ownership.

### Scoped lowering and ownership

Extend `compile_read_in`; do not add a public prepared-query API. Add
`crates/zeppelin-embed-cypher/src/lowering/search.rs`, a `DraftOp::Search`, and a
charged `Buffer<PlanNodeId>` for eager inventory in `lowering/mod.rs`.

Lower each call as follows:

1. Lower its typed request expressions through the existing sparse expression
   remap. Map `AllIndexed` to `None`; map both materialized forms to
   `Some(lowered_expr)`.
2. For unrestricted or literal-empty calls, create an independent
   `Unit -> Search`. If prior bindings exist, combine the current root and this
   source with `Join { predicate: None }`. This is the required Cartesian
   composition; chaining the new search directly after prior rows would fail the
   core singleton guard and would incorrectly make it row-correlated.
3. For `GlobalDistinctNodes`, use the current global-aggregate root as the
   `Search` input. Core rechecks that the input is singleton, and the evaluated
   eligible slot remains in scope.
4. Append every `Search` node to `eager_searches` at index `SearchCallId`, even if
   all its output slots are later projected away. The existing runtime barrier
   then preserves invocation and report order through empty inputs and LIMIT 0.
5. Freeze the eager buffer with the existing expression/operator/fact arenas,
   add its real region and retained owner, and adjust fixed region/owner
   capacities. Compilation, plan validation and the callback continue to use the
   one supplied `QueryMemory` and sealed `ReadContext`; no account, view, lease or
   runtime source is created here.

This changes only:

- `crates/zeppelin-embed/src/property_graph/query/plan/{mod.rs,search.rs,validate.rs}`
- `crates/zeppelin-embed-cypher/src/binding.rs`
- `crates/zeppelin-embed-cypher/src/binding/search.rs`
- `crates/zeppelin-embed-cypher/src/lowering/mod.rs`
- new `crates/zeppelin-embed-cypher/src/lowering/search.rs`
- focused plan/binding/search-lowering/allocation tests and the corresponding
  changed-path adversarial probe

`query/runtime/driver.rs`, `query/eligibility.rs` and `query/completed/**` are
consumers, not component-owned redesigns.

## Component acceptance

This component is complete only with literal RED then GREEN for these focused
boundaries:

1. Core plan facts expose the exact output kinds for all three procedures,
   including nullable hybrid components; wrong fields, duplicate slots and
   malformed eager inventories reject.
2. `'default'`, `'auto'`, `'exact'` and `'scan'` remain four distinct request
   values through the final validated `GraphPlan`; no request is represented as
   actual candidate coverage and `'approximate'` remains rejected.
3. Omitted eligibility lowers to `None`; literal `[]` lowers to a reachable list
   expression in `Some`; global `collect(DISTINCT node)` plus singleton aliases
   lowers to its current-scope slot. Grouped, nondistinct and per-row forms remain
   rejected.
4. Literal, parameter and vector-list arguments passed through one or more
   singleton aliases lower to their canonical expression with no preceding-row
   `Slot` dependency. The independent source retains their exact value even when
   the preceding binding stream is empty, and its eager invocation remains
   prepare-able once.
5. Two independent calls after MATCH lower to two separate singleton-root search
   sources plus Cartesian joins, with call IDs and eager inventory `[0, 1]`.
   Projection of every score/component still retains both inventory entries.
6. A dependent eligible call uses the singleton global aggregate directly and is
   not rebuilt per input row. An empty prior branch cannot suppress preparation
   of a later independent source at the plan/runtime barrier seam.
7. Exact source/operator spans, final columns, sparse expression reachability and
   no-escape lifetimes remain intact. Every allocation/control failure returns no
   consumer and restores the original query/shared reservation baseline.
8. Deliberate mutations of mode mapping, component nullability, empty-versus-
   omitted eligibility, second eager identity and independent-source Join each
   fail, are restored, and receive same-seed clean controls.

These tests prove a compiler-to-validated-plan component. They do not fabricate
search rows, admission, ranking, reports or public execution.

## Remaining integration ownership and unchanged gates

- ZE-64 still owns the real same-view vector/text/hybrid adapter, actual tier and
  coverage, nullable component population, eager `prepare_search`, Cartesian row
  production, per-call reports, full-u128 ordering and work/cancellation under
  one `RuntimeContext`.
- ZE-51/53 still own real evaluator/operator composition and completed native
  result production. The existing report arena is reused rather than duplicated.
- ZE-56 retains public compiler-to-GraphStore read acceptance; ZE-58 retains the
  full three-shape search-call acceptance, including actual row bags and report
  counts; ZE-59 retains full profile qualification. Plan construction increments
  no executed TCK or application-shape count.
- ZE-118 retains broad workspace/adversarial/coverage campaigns. This component
  runs only its directed RED/GREEN, allocation/control and changed-path seeded
  checks before integration.

The scheduler must therefore use one owner for the shared IR plus scoped CALL
lowering, then let ZE-64 consume that compiled commit. Parallel edits to
`plan/mod.rs` or separate compiler work against an uncompiled proposed IR would
recreate the mismatch this audit identifies.

## Mutation-lowering safety note

No new mutation restriction is needed. The parser already rejects `CALL` before
or after any updating clause, before binding, lowering or writer admission.
([parser.rs:233-292](../../../crates/zeppelin-embed-cypher/src/parser.rs#L233-L292))
The search component should retain those exact rejection tests and must not add a
mutation lowerer or an eager write barrier.
