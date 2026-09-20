# Native execution readiness after ZE-145

ZE-148 bounded source review, 2026-09-20. Inspected main pin
`ce7c5f948e60287697b395e1b27d6363b9f79b6b` in the assigned research worktree.
This is a recommendation for root review, not a scheduling decision or execution
evidence. No build, test, product edit, commit, tracker mutation or ZE-39 worktree
inspection was performed. Canonical ignored planning documents were read from
the main checkout; production source references below use the pinned worktree.

## Readiness result

**Do not schedule complete pattern or relational native consumers yet. Schedule
at most one narrowly bounded error-transport prerequisite if root approves the
additive contract below.** No second implementation is proposed.

ZE-145 removed the missing scalar-evaluator prerequisite recorded by ZE-143.
Its actual result cannot yet travel through the existing pull/relational driver
without losing its expression identity or typed storage/plan cause. Native scans
and expansion have the same transport mismatch. ZE-143 intentionally left this
decision to actual operator integration; its instruction is not a prohibition
on the next owner resolving it.

The proposed prerequisite corrects that concrete mismatch in the existing driver
and wrappers. It does not implement a native pattern source, an expression
projection operator, a planner or a second execution framework. A complete native
consumer must wait for this interface to compile and integrate; it must not be
assigned concurrently against the proposed signatures in this document.

## Source facts

`src/` in this table means `crates/zeppelin-embed/src/`.

| Source at the pin | Available contract or exact obstruction |
|---|---|
| `src/property_graph/query/expression.rs:90-111,506-558,570-607` | The real evaluator exists. Construction returns `ExpressionFailure::{Runtime,Plan,Tree}`; evaluation returns `ExpressionError { expression: ExprId, failure }`. It consumes retained plan/parameters, explicit schema/rows, admitted view and the original runtime. Its borrowed successful value has charged scratch backing. |
| `src/property_graph/query/runtime/driver.rs:23-38,63-70,127-135,182-190` | `PullOperator::pull`, `prepare_search`, `OperatorFactory::build`, `Completion::complete` and `RuntimeFailure.error` currently hard-code `RuntimeError`. |
| `src/property_graph/query/runtime.rs:155-189` | `RuntimeError` has only `Limit`, `Value`, `Memory`, `Batch`, `IdentityExhausted`. There is no lossless storage/plan/expression payload. |
| `src/property_graph/storage/tree/directory.rs:16-37` | `TreeError` already contains `RuntimeError` by value, along with exact missing/invalid/format/WAL/control/I/O/memory/work causes. Putting `TreeError` or `ExpressionError` directly inside `RuntimeError` would create a recursive by-value type. |
| `src/property_graph/query/relational.rs:156-160,224-252,293-312`; `query/relational/blocking.rs:26-56,155-198` | `RowOperator`, `MapRows` and `BlockingRows` inherit the hard-coded pull error. ZE-125 supplies genuine charged row/kernel composition, but consumes pre-evaluated columns and cannot preserve a native child failure today. |
| `src/property_graph/query/runtime/driver.rs:249-294,297-415` | Owned factories and existing-context execution share one `drain`: eager sources, pull batches, private collector, completion and final control. Preserve this one path, its counters and its no-output-on-error behavior. |
| `src/property_graph/storage/view.rs:143-224,284-334`; `storage/view/expression.rs:7-34` | Real admitted node/relationship reads, both-endpoint liveness, same-view cursor scans/expansion, ownership checks and catalog resolution are compiled. They return `TreeError`; this review found a transport obstruction, not absent scalar/read data. |
| `src/lifecycle/native_graph.rs:69-75,1047-1083` | `NativeReadConsumer` returns `TreeError`; `with_native_read` owns actual admission/source/catalog/runtime/final check. Its later execution-error adapter stays outside this prerequisite. This file is owned by active ZE-39. |
| `src/lifecycle/native_graph.rs:2717-2824` | The existing test-only `NativeNodePull` implements the old trait and maps native errors to `RuntimeError::Batch`. This is component test plumbing, not an acceptable production consumer template. A fixed replacement of the trait return type would force edits here and violate disjoint ownership. |
| `tasks/evidence/ze-143/README.md:7-11,45-47`; `tasks/evidence/ze-145/README.md` | The prior split preserves original integration acceptance, explicitly defers error transport to operator integration, and records integrated scalar component verification. Those recorded results were not rerun in this review. |
| Main `docs/graph/plans/execution.md:19-23,49-69,95-101,134-154`; `parallel-contracts.md:16-24,40-54,150-156` | Typed causes/source identities, same-owner accounting, native composition, error-preserving predicates, single-owner compiled handoff and original real-producer acceptance are required. In particular `parallel-contracts.md:44` prohibits collapsing typed native errors into `Batch`. |

ZE-39's approved `/tmp/ze-39-astra-execution-plan.md:35-60` owns publication,
native lifecycle, prepared/base accessors, staging handoff, WAL/VFS, its native
test child and runner additions. None of the five production files below is in
that ownership list. No change to `native_graph.rs`, including module declarations,
is part of this proposal.

## One proposed flat prerequisite ticket

**Title:** Preserve typed native failures through graph execution

Epic E5; type story; themes query, graph, correctness; label implementation.
Prerequisites: ZE-29, ZE-45, ZE-48, ZE-49, ZE-125, ZE-145 and completed ZE-148
review. All production prerequisites are integrated. Root adds this ticket as a
mandatory prerequisite of ZE-50 and ZE-51, retaining every existing edge. This
does not start either blocked original. Root records the accepted addendum in
the execution and parallel-contract plans and E5 decision index, then exports
the tracker. Research alone does not approve those changes.

Proposed description:

> Carry the existing native evaluator and storage failures through the existing
> pull driver, factory, completion and relational wrappers without erasure or a
> second driver. Preserve the exact RuntimeError, ExpressionError with ExprId,
> PlanError and TreeError payloads; leave RuntimeError and its current consumers
> unchanged. Use defaulted error type parameters only at these transport seams,
> and a single private concrete native error union. Existing callers continue to
> select RuntimeError without source edits. No public GraphStore error/API freeze,
> native source, pattern/relational expression consumer, admission adapter,
> publication, compiled-plan executor or generic callback registry is included.
>
> Observe a behavioral RED for a lost typed diagnostic through the real drain,
> then prove lossless transport through that same drain and real MapRows and
> BlockingRows, including a late failure after earlier private rows, no completed
> output, exact consumed counters and released reservations. Prove unchanged
> RuntimeError callers and feature/consumer compilation. Tests of deliberately
> injected errors establish transport only; they are not native graph producer
> or integration acceptance. Use this source-grounded ZE-148 plan and
> /tmp/graph-sol-executor-rules.md. Root integrates and closes. Broad execution
> remains ZE-118; every original ZE-50/51 criterion remains mandatory.

## Exact additive contract for root review

This is a bounded correction to existing signatures, not a proposal to make all
query/storage errors generic.

1. Add an error type parameter with default `E = RuntimeError` to
   `PullOperator`, `OperatorFactory`, `Completion`, `RowOperator` and
   `RuntimeFailure`. `RowOperator<..., E>` extends `PullOperator<..., E>`;
   `OperatorFactory<..., E>::Operator<'v>` implements that same pull type.
   Only those trait methods already returning `RuntimeError` change to `E`.
   `RuntimeFailure<E>.error` retains `E`; its existing operator/counter fields
   remain. Error `Display`/`Error` implementations use the necessary ordinary
   generic bounds.
2. Thread the same inferred `E: From<RuntimeError>` through `execute`,
   `execute_factory`, `execute_in` and the **existing single `drain`**. Preserve
   eager source order, private collection, capacities and final checks verbatim
   apart from explicit error conversions. Convert direct `MemoryError` through
   `RuntimeError` before `E`; `?` does not perform two `From` conversions. Do not
   widen `RuntimeError`, `RuntimeContext`, `RowBatch`, `FrozenOutput` or storage.
3. Add defaulted `E = RuntimeError` to `MapRows` and `BlockingRows` and their
   implementations. A zero-sized `PhantomData<fn() -> E>` is sufficient if the
   compiler requires a retained type marker; it supplies no owner or budget.
   Child errors pass unchanged; local schema/batch/value/kernel errors convert
   from their existing `RuntimeError`. `BlockingRows`' internal `RowSource`
   stays a `RuntimeError` producer and its output pull is explicitly converted.
   Keep `Rows`, `RowSource`, `Rows::into_source`, schemas, aggregate/distinct/sort
   kernels and eligibility APIs nongeneric and unchanged. This avoids giving
   existing sources multiple error implementations and destabilizing inference.
4. Add exactly one crate-private `NativeExecutionError` enum with the actual
   existing payloads `Runtime(RuntimeError)`, `Expression(ExpressionError)`,
   `Plan(PlanError)`, `Tree(TreeError)`. Preserve expression identity and every
   nested cause. `From<ExpressionFailure>` maps its three existing variants
   directly; `From<ExpressionError>` preserves the complete expression wrapper.
   No box, allocation, string conversion, sentinel `Batch`, future write error,
   public language span or guessed result shape is needed. `RuntimeError`
   remains a leaf relative to this outer union, so there is no recursive type.
5. Defaulted trait/struct parameters preserve existing implementations. Existing
   driver function callers infer `E` from their concrete source/factory and
   completion implementations. The bounded source search found no explicit
   `execute::<...>`, `execute_factory::<...>` or `execute_in::<...>` invocation
   requiring a changed type-argument count. Compilation is still an acceptance
   gate, not a claim established by that search. Do not compensate for inference
   trouble by adding adapters, default-success implementations or a second API.

The outer union stays private even though the existing transport traits are
public Rust component interfaces. There is no need to expose the private scalar
evaluator or choose the future public GraphQueryError representation.

## Maximum file ownership

At most these **five production files** may change:

- `crates/zeppelin-embed/src/property_graph/query/runtime.rs`: declare/re-export
  the graph-gated private native-error module only; leave `RuntimeError` intact.
- `crates/zeppelin-embed/src/property_graph/query/runtime/native_error.rs`: new
  concrete error union, exact conversions and formatting/source access.
- `crates/zeppelin-embed/src/property_graph/query/runtime/driver.rs`: additive
  transport parameters/conversions, its existing compile-fail example if needed,
  and a new test-child declaration.
- `crates/zeppelin-embed/src/property_graph/query/relational.rs`: only transport
  parameters/conversions/zero-sized marker for `RowOperator` and `MapRows`.
- `crates/zeppelin-embed/src/property_graph/query/relational/blocking.rs`: only
  corresponding `BlockingRows` transport and its internal RowSource conversion.

Additional owned files: new
`crates/zeppelin-embed/src/property_graph/query/runtime/driver/error_transport.rs`
for graph-gated unit tests and `tasks/evidence/<assigned-ticket>/README.md` for
brief exact evidence. No modifications to existing integration tests, native
fixtures, compiler, FFI, adversarial source, Cargo files, default feature boundary,
plan IR, evaluator, storage or inherited dirty files. If the five-file boundary
cannot compile existing callers unchanged, stop and return the exact signature
and diagnostic; do not grow the allowlist locally.

## Narrow executor plan

Root assigns one Sol/xhigh executor in a fresh worktree from the reviewed main
pin, after approving/recording the ticket. No other native consumer starts before
this prerequisite integrates. Apply `/tmp/graph-sol-executor-rules.md` throughout.

1. Establish a behavioral transport RED through the current real `execute_in`
   drain: a test-only source retains a known expression failure with nonzero
   ExprId and a distinctive `TreeError::Invalid` detail, emits an earlier private
   row, then returns the only current representable lossy diagnostic (`Batch`).
   Normalize the observed diagnostic into an independent test observation and
   assert the original ExprId/cause/detail; the intended mismatch must execute.
   Do not count an unavailable-type compile failure as the behavioral RED. The
   test source is controlled error injection, not a graph producer. Keep the
   expected diagnostic fixed when the source is migrated to the lossless trait.
2. Implement the concrete private union and additive driver transport first.
   Prove the same test GREEN using an actual
   `PullOperator<..., NativeExecutionError>` through the original drain, with
   no lossy adapter left. Keep a plain `PullOperator<..., RuntimeError>` case
   in the same test module as an inference and behavior control. This is the
   first acceptance milestone; report RED, compilation and GREEN to root.
3. Thread the error through the two existing relational wrappers. Complete these
   new named tests, using real QueryMemory/RuntimePlan/RowBatch reservations and
   real MapRows/BlockingRows rather than alternate kernels:
   - `native_error_transport_late_pull_preserves_expression_and_tree`: nonzero
     ExprId, exact runtime/plan/tree expression causes, distinct storage detail,
     prior privately collected rows, failure contains no output, completion was
     never called, counters equal actual work and reservations return to baseline.
     Include a direct TreeError and retained I/O error kind/code; do not compare
     only rendered strings.
   - `native_error_transport_relational_wrappers_preserve_child_failure`:
     streaming map and blocking chain propagate the exact child error after
     consumed input; local filter/type/work errors remain Runtime causes.
   - `native_error_transport_factory_eager_and_completion_preserve_failures`:
     factory construction, eager preparation before any pull, and late completion
     refusal each retain the exact error payload and existing diagnostic operator;
     no success or completed output escapes. Later eager sources are not invoked
     after failure. Drop real retained buffers on each path.
   - `native_error_transport_default_runtime_and_close_first_are_unchanged`:
     unchanged default caller types, exact RuntimeError pattern matching, and
     both default/outer-error paths preserve actual value/control close-before-
     caller-cancel precedence and cumulative tightened work-limit identity.
4. Run only the new tests and these affected regression selections:

   ```sh
   cargo nextest run --profile default -j 4 --retries 0 -p zeppelin-embed --features graph-cypher --lib -E 'test(native_error_transport_)'
   cargo nextest run --profile default -j 4 --retries 0 -p zeppelin-embed --features graph-cypher --test graph_query_runtime_control -E 'test(factory_retains_real_buffers_through_three_stages_and_many_batches) | test(factory_composes_production_relational_owners_and_releases_failed_preparation) | test(failed_last_pull_and_completion_cancellation_never_expose_partial_output) | test(close_after_last_pull_drains_the_same_lease_and_precedes_caller_cancel) | test(eager_sources_run_once_in_source_order_even_when_limit_zero_produces_no_rows)'
   cargo nextest run --profile default -j 4 --retries 0 -p zeppelin-embed --features graph-cypher --test graph_relational -E 'test(sparse_slots_filter_project_and_limit_preserve_bags_across_batches) | test(blocking_chain_sorts_then_collects_across_many_scheduling_batches)'
   ```

   Require nonzero selected counts and zero retries. New unit tests deliberately
   isolate transport; they establish no real native scan, publication or TCK
   evidence. No broad/full/adversarial execution is authorized.
5. Required compile controls for unchanged actual consumers:

   ```sh
   cargo check -p zeppelin-embed --lib
   cargo check -p zeppelin-embed --features graph-cypher --lib
   cargo check -p zeppelin-embed-cypher --test runtime_lowering
   cargo check -p zeppelin-embed-ffi --lib
   cargo check -p zeppelin-embed-ffi --features graph-cypher --tests
   cargo check -p zeppelin-embed-ffi --features graph-result-test-support --tests
   cargo check -p zeppelin-embed-workspace-tests --test adversarial_tests
   cargo check -p zeppelin-embed-workspace-tests --features graph-cypher --test adversarial_tests
   cargo check -p zeppelin-embed-workspace-tests --features graph-result-test-support --test adversarial_tests
   git diff --check
   ```

   Core lib-test compilation from the new nextest command includes the unchanged
   native_graph.rs trait implementations. Compiler/FFI/runner checks cover their
   existing PullOperator/Completion implementations and direct RuntimeFailure
   error matches. These are compile controls, not runner execution. Broad runner,
   coverage, performance and platform evidence stays on ZE-118. Record that this
   transport-only change adds no production native operation or fault site;
   future native consumers must register their real changed work/failure seams.
6. Stop after acceptance/checks pass. Inspect the five-file production boundary,
   format only owned files, preserve inherited hashes, and commit only ticket
   paths with the assigned ZE key plus literal RED/GREEN evidence. No push,
   tracker edit/close or main integration; root owns those actions.

Stop immediately for an edit outside the allowlist, necessary lifecycle/source
ownership change, failure-payload erasure, boxed/unaccounted error allocation,
ambiguous unchanged caller inference, budget/feature change or a second execution
path. **Two unsuccessful fixes of the same issue, or 20 minutes without an
acceptance milestone, require the exact command/error, diagnosis and smallest
next action to root.** Do not turn a generics/inference problem into an open-ended
framework refactor. Root may decline this prerequisite and leave transport to
the original integrated operator work.

## Acceptance retained by the originals

| Owner | Requirement still mandatory after this prerequisite |
|---|---|
| ZE-50 | Real key/label sources, directional and bounded expansion, both-endpoint liveness, source/target identity, per-MATCH relationship uniqueness across joins, zero-hop/list and both edge-predicate phases, joins/OptionalApply, selective planner, independent exact tiny-graph tuples and legal permutations, genuine compaction/reopen. ZE-46 remains a blocker. |
| ZE-51 | Real ZE-50 producer plus ZE-145 expression-to-relational composition; filter/project/WITH scope, aggregates, null/NaN/numeric equivalence, ordering/bags, limits/work and actual singleton eligible-set construction. ZE-125 kernel evidence is unchanged. |
| ZE-52/53/56/66 and later originals | Overlay/progressive mutations, real completed/public structured result path, public read/TCK execution, GraphStore facade/admission and outer error conversion, copied result lifetime, ABI and all original integration criteria. |
| ZE-39/40/46 | Durable publication, recovery/reopen and real compaction remain wholly their owners' work; controlled component fixtures never replace them. |
| ZE-118 | Deferred full/workspace/adversarial/coverage/performance/platform qualification, including actual new native consumer failure/ordering seams once those consumers exist. |

The transport story, if accepted, establishes only a lossless, compiled transport
contract for the next native operator owner. It does not itself certify that all
remaining pattern or relational composition seams are ready, and this bounded
review does not propose another readiness campaign or an extra worker ticket.
