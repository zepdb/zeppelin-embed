# ZE-143: native expression execution readiness

Inspected integrated main `439237bd382e6186cf6ba869acf0d817284b8937` on 2026-09-20. ZE-143 is claimed. This is source-backed planning only: no product edit, build, test, or new ticket was performed. Existing dirty main paths were preserved.

## Decision

**GO for one flat implementation ticket: implement complete native scalar expression evaluation. Do not schedule a second pattern/relational consumer in parallel.** The actual producers needed by this evaluator are integrated. Full pattern execution is not independently ready against a compiled evaluator: none exists, and ZE-125 deliberately consumes already evaluated slots. A single useful evaluator is a better boundary than concurrent workers inventing that interface.

This follows the owner's existing authorization for safe implementation splits. Root creates/schedules the proposed ticket, adds it as a prerequisite of ZE-50 and ZE-51, and records the scheduling addendum. Preserve every old dependency and acceptance criterion. ZE-50 retains actual pattern operators, joins/optional/uniqueness, planner and genuine compaction/reopen; ZE-51 retains expression-to-relational and real pattern composition. ZE-52 retains overlay/progressive mutation expression integration. ZE-53/56 retain public completed results and read execution/conformance. ZE-118 retains broad qualification.

Read `/tmp/graph-sol-executor-rules.md`; those operating limits apply. Executor: fresh main-based worktree, GPT-5.6-Sol/xhigh, sole implementation owner. No consumer may use this evaluator until its actual interface compiles, passes its focused checks, and is integrated.

## Existing producer contracts

All paths below are relative to `/Users/aghatage/Documents/code/zeppelin-embed`; line references are at the inspected main revision.

| Existing source | What is available now |
|---|---|
| `src` below means `crates/zeppelin-embed/src/` | Core consumes structured data only; no compiler dependency is added. |
| `src/lifecycle/native_graph.rs:69-75,1047-1083` | `NativeReadConsumer` receives the actual `GraphReadView` and the original `RuntimeContext`; `Store::with_native_read` owns admission, lease, memory, catalog/source and final close-first check. |
| `src/property_graph/storage/view.rs:137-261` | Real live node/relationship lookup, both-endpoint relationship liveness, property payloads and optional stored text; errors remain typed. Text can be copied in bounded chunks (`92-106`). |
| `src/property_graph/storage/records/native.rs:62-104` | Validated source-bound record shape, labels and exact canonical property encoding. `records.rs:298-343` pins scalar/list tags and geometry. `storage/stream.rs` provides bounded payload reading. |
| `src/property_graph/storage/view/catalog.rs:16-22,122-160`; `catalog.rs:296-333` | The admitted catalog already owns the real symbol table. Its existing `SymbolCatalog::lookup/name` support exact read-only name resolution. The native adapter needs thin accessors; no new catalog producer or interning is needed. |
| `src/property_graph/query/plan/mod.rs:78-184,638-743` | All scalar expression forms, logical slots, immutable validated plan/facts and parameter validation exist. Aggregate expressions are explicitly distinct from scalar functions. |
| `src/property_graph/query/relational.rs:16-64`; `runtime/batch.rs:335-458` | Explicit logical-to-physical schema and authentic owned rows; copying a final value into a row is already bounded and transactional. |
| `src/property_graph/query/{value,scalar,id_text,property,list}.rs` | Existing truth, arithmetic, exact comparison, membership, indexing, string, ID text, stored/query conversion and bounded list semantics. Reuse these kernels. |
| `src/property_graph/query/runtime.rs:29-73,220-303`; `resources/inputs.rs:148-264,364-382` | One cumulative control/work/memory owner, retained input capacities, RuntimePlan ownership and authentic QueryArena storage. |
| `crates/zeppelin-embed-cypher/src/lowering/mod.rs:30-64` | The real compiler already retains complete plan/fact/parameter owners and exposes typed parameter bindings. It is a future consumer of core evaluation, not an input contract that needs inventing. |
| `src/lifecycle/native_graph.rs:1775-1858,2154-2159,4569-4723` | Existing tests prepare real native participants, install a controlled bundle, admit it, and combine native reads with authentic plan/row memory. These are reusable fixture patterns, not durable-publication acceptance. |

Accepted behavior is in `docs/graph/plans/execution.md:25-45,95-101,123-126,134-142` and `docs/graph/plans/cypher.md:62-65`. `docs/graph/plans/parallel-contracts.md` keeps component evidence distinct from original integration acceptance. This plan does not require refreshing external reference research.

## Complete boundary to implement

Add a crate-private native scalar evaluator. It evaluates a selected scalar `ExprId` from a retained validated plan against one explicit `Schema`/`RowBatch` row and validated retained parameters, inside the supplied authentic native read scope. It must be directly usable by future pattern predicates and relational expression projection; do not make it a test-only function or a success-returning placeholder.

Support every non-aggregate `Expression` variant and every declared scalar unary/binary operation: literal, logical slot, parameter, heterogeneous/nested list, property, HasLabel, three-valued Boolean operations, comparisons, arithmetic, string predicates, IN/index, size, labels, relationship type, stored text and full-width ID strings. `Expression::Aggregate` is a typed wrong-evaluation-context refusal; aggregate execution remains the existing relational owner's responsibility. Do not add new functions or language forms.

Keep outputs in an accounted, reusable private scalar/list/string scratch owner. Borrowing the successful value prevents resetting its backing; the caller can copy it into the existing RowBatch before reuse. Use QueryArena and the existing bounded QueryList/QueryValue machinery, checked offsets and bounded expression frames; do not retain self-referential vectors, extend lifetimes, leak pointers, or build a second general execution framework. Output is accessible only after complete success, and failed evaluation cannot expose partially initialized lists/text or alter a caller's published destination. Counters retain work already performed.

Resolve symbols from the exact admitted catalog. Unknown property on a live entity yields Null; unknown label is false on a live node; null receiver yields Null for property/label/scalar native functions. Missing/deleted/nonvisible entity reads fail explicitly, rather than fabricating an empty record or treating corruption as absence. Relationship reads retain both-endpoint liveness. ID-text is identity bookkeeping: after same-view/kind validation it performs no existence lookup. Preserve full-u128 identity, exact F64 bits and stored empty-list meaning.

Read native canonical property encodings through bounded PayloadCursor/PayloadSlice operations into the same charged scratch; cover all scalar and homogeneous list types. Do not copy complete entity records to get one property or use raw mmap alignment casts. StoredText distinguishes missing optional payload, present empty, and zero-term text, and copies actual UTF-8 chunks under the same accounting/control. No full-vector expression is added.

Plan, input row, scratch, parameters, native source and catalog must belong to the same runtime/query owner. Validate actual retained parameter owners using QueryInputs/RetainedAllocation or copy from proven retained owners into charged evaluator storage; never accept raw slice length or an asserted byte allowance as actual-capacity evidence. Reuse GraphPlan parameter validation, including missing/duplicate/wrong-type and nested-entity rejection. Small private proof accessors may expose existing ownership checks; they must not weaken them. Parameter values remain immutable for the evaluation.

Charge `WorkKind::Expressions` at each requested scalar evaluation site and charge/check real recursive value, list, byte-copy and native read work cumulatively through the supplied RuntimeContext. Reuse value-kernel accounting; do not reset a ValueContext or create a per-expression budget. Preserve close-first control and typed memory/work/arithmetic/storage errors. Use a local evaluator error that retains existing RuntimeError/PlanError/TreeError and expression identity; **do not widen RuntimeError or refactor PullOperator in this ticket**. Those transport decisions belong to actual operator integration. Do not cache native property results across rows/clauses or optimize away error-producing expressions.

## File ownership

Allowed production additions/edits only:

- `crates/zeppelin-embed/src/property_graph/query/expression.rs` and its new `expression/` children: complete evaluator, private scratch/error/input handling.
- `crates/zeppelin-embed/src/property_graph/query/mod.rs`: module declaration only.
- `crates/zeppelin-embed/src/property_graph/query/list.rs`: only a small private backing/ownership helper if needed to authenticate nested parameter storage; no value semantic change.
- `crates/zeppelin-embed/src/property_graph/query/resources/inputs.rs`: only small private delegation to existing retained-span/owner verification if needed; no allowance relaxation.
- `crates/zeppelin-embed/src/property_graph/storage/view/expression.rs`: new GraphReadView expression accessors over its existing lease/source/catalog/records.
- `crates/zeppelin-embed/src/property_graph/storage/view/catalog.rs`: exact read-only symbolic lookup/name accessors over the existing image; preserve control error identity.
- `crates/zeppelin-embed/src/property_graph/storage/view.rs`: `mod expression;` declaration only.
- `crates/zeppelin-embed/src/lifecycle/native_graph.rs`: add a test-child module declaration inside the existing `tests` module only.
- `crates/zeppelin-embed/src/lifecycle/native_graph/tests/expression_tests.rs`: native evaluator tests and any additional small real-participant fixture required.
- `tasks/evidence/<assigned-ticket>/`: concise RED/GREEN and final check logs.

The native test child can access existing private parent fixture helpers. Reuse them unchanged when sufficient; keep extra fixture construction in the new child and use the actual staging/participant producer. No extraction/reorganization of the large native fixture module.

ZE-60 planner explicitly confirmed disjoint ownership: its additions go in `storage/view/retrieval.rs`, with only `mod retrieval;` in `view.rs`; it does not change `catalog.rs`. Root reconciles the two additive module declarations. Do not depend on its uncompiled runtime/view accessor: the expression adapter validates its exact existing lease/source/runtime/memory itself. Do not modify retrieval, compiler, pattern, relational, public API/ABI, publication, compaction, recovery, runtime driver, limits or Cargo files.

## Ordered implementation and focused acceptance

1. Add named evaluator tests against the existing typed/native fixture contracts and observe the intended RED. First prove a complete scalar evaluation copied into real RowBatch backing using the actual admitted RuntimeContext; then retain that working ownership shape throughout implementation. Do not claim absent-symbol compile failure alone as semantic evidence.
2. Implement the private scratch/input/error boundary and all scalar dispatch using the existing kernels. Add the thin symbolic/property/text native adapter and complete every listed expression form. Reject scalar Aggregate explicitly. No scheduler, optimizer or generic provider trait is required.
3. Exercise the completed evaluator against actual producer files and native admission with these focused test groups (use these `native_expression_` prefixes):
   - `native_expression_scalar_dispatch_and_parameters`: all scalar variants, sparse logical SlotIds, null/Boolean tables, exact large I64/F64 comparison, checked arithmetic errors, Unicode strings, lists/indexing/nesting, parameter failures, scalar-aggregate refusal. Expected outcomes are literal cases, not calls to the production evaluator as oracle.
   - `native_expression_reads_real_properties_labels_types_and_text`: both entity kinds, every stored property kind/empty list, missing property/null receiver, unknown labels, full IDs sharing low bits, both-endpoint-hidden relationship rejection, labels/type, absent/empty/zero-term and multi-chunk text. Native error and absence must remain distinct.
   - `native_expression_preserves_view_and_output_ownership`: use actual same-view rows, reject equal-metadata foreign admission and same-view/different-runtime or foreign-memory usage before reading; controlled replacement during the admitted read still sees the old properties/text; copied scalar/list/text output survives source scratch reuse. Use the existing completed scalar owner where needed to verify a copied string after close; do not claim public result completion or compaction/reopen.
   - `native_expression_limits_cancellation_and_failure_are_atomic`: cumulative repeated evaluation reaches an exact tightened Expressions limit; owned bytes are reserved before allocation; wrong owner and oversized output fail; late expression/native-read error and cancel during text/list work expose no value or appended destination row and release charges; actual close takes precedence over caller cancel. Include a literal known-work case and an independent expected-value negative control.
4. Terminal focused checks only:
   ```sh
   cargo nextest run --profile default -j 4 --retries 0 -p zeppelin-embed --features graph-cypher --lib -E 'test(native_expression_)'
   cargo nextest run --profile default -j 4 --retries 0 -p zeppelin-embed --features graph-cypher --lib -E 'test(native_read_scoped_consumer_retains_one_catalog_and_bundle) | test(native_read_cursor_rejects_same_view_memory_different_runtime)'
   cargo check -p zeppelin-embed --lib
   cargo check -p zeppelin-embed --features graph-cypher --lib
   git diff --check
   ```
   The no-feature check protects the existing opt-in boundary. Because this allowlist does not change RuntimeError or FFI, no optional-hook build is required unless root authorizes a necessary cross-boundary edit. Run formatting only on owned files. No full/adversarial/coverage/fuzz/size/performance run. Record newly introduced fault/work seams for ZE-118; do not create an unrelated runner campaign here.
5. Stop when the listed acceptance passes, inspect allowlist, commit only ticket-owned files and report commit/checks/limitations to root. Root integrates and closes. No broad check may be used to replace the missing focused acceptance.

If the first real admitted input/scratch/output chain cannot compile without changing a shared runtime/RowBatch contract, or the actual property/catalog producer is insufficient, stop and send root the smallest exact blocker. The same failure surviving two attempted fixes also triggers the root escalation rule. Do not route around that blocker with fake records, uncharged copies, ad hoc budgets, a replacement evaluator interface, or partial operation coverage. Boolean error-elision/short-circuit behavior is not specified by the existing truth-table kernels; use ordinary evaluation of both operands without an optimization, and escalate any acceptance conflict instead of inventing an optimizer rule.

## Proposed flat ticket text for root

Title: **Implement native graph scalar expression evaluation**

Epic E5; type story; themes query, graph; labels implementation. Prerequisites: ZE-29, ZE-45, ZE-48, ZE-49, ZE-123, ZE-125, ZE-126, ZE-143 (all compiled prerequisites plus this reviewed readiness decision). Add this ticket as a required prerequisite of ZE-50 and ZE-51; retain every existing edge. No second new implementation ticket is proposed.

Description:

> Implement one complete crate-private native scalar expression evaluator over the integrated typed Expression/RuntimePlan, explicit Schema/RowBatch, retained parameters, QueryValue kernels, authentic RuntimeContext and admitted GraphReadView. Own all nonaggregate scalar forms, including actual properties/labels/type/stored text, nested lists, null/type/arithmetic behavior and full-width ID text. Aggregate-in-scalar context rejects explicitly. Native data errors remain typed; no new runtime framework, public API or future-producer interface.
>
> Use `/tmp/ze-native-execution-parallel-plan.md` and `/tmp/graph-sol-executor-rules.md` as the bounded executor handoff. Exact ownership is new query/expression and storage/view/expression modules; narrow existing module/catalog/input-proof edits; new native test child; ticket evidence. ZE-60 owns disjoint retrieval files; root reconciles only additive module declarations.
>
> Acceptance: literal scalar/parameter cases and all real stored property kinds; exact native label/type/text/identity behavior; actual same-view/runtime/memory validation; old admitted view under controlled replacement; owned copied scalar/text output; cumulative checked work/memory and close-first cancellation; late error without exposed partial value/destination mutation and complete reservation release. Observe meaningful RED, then the named focused native_expression tests and adjacent view checks GREEN. No broad/adversarial tests; final broad obligations remain ZE-118.
>
> This independently usable production evaluator does not complete pattern execution, relational plan composition, write-overlay visibility, durable publication, real compaction/reopen, public results, TCK or ABI. ZE-50/51/52/53/56 and all existing prerequisites/acceptance remain mandatory. No parallel consumer uses this interface before compilation, focused verification and integration. Root owns tracker scheduling, plan addenda, backup, integration and close.
