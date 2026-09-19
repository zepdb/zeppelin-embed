# ZE-126 independent frontend resource/control review

Final disposition: the copied-parameter UTF-8 blocker is resolved by the independently reviewed frozen correction below. No remaining concrete blocker in this bounded frontend resource/control review. The original observation and exact prior measurements are retained for provenance; own-adapter and acceptance exclusions still apply.

## Snapshot and independence

Reviewed `/tmp/ze-126-semantic-review-1` (31/31 file hashes match; inventory SHA-256 `2c6e01af32886494503d6e6c61fde0e88b0f2349e144e32f92fe417627264155`) and supplement `/tmp/ze-126-control-review-1` (4/4 match; inventory SHA-256 `5bbef6172d2c451d093e5b683770ea3fc6de88b5b5fdb1ba996405cd04ded059`). The supplement supplies the final four runtime tests and original-control accessors.

Scratch `/tmp/ze-126-resource-review` was assembled from immutable imported HEAD `beb1a1e44730b56494ec2cb4aafc5d9ea5419763`, the root-accepted core adapter frozen files, and these two snapshots. `/tmp/ze-126-resource-review/review-assembly.json` records all 36 resolved path hashes; all still match after probing. No live worktree, main, tracker, or product source was mutated.

Excluded from independent certification: my four previously contributed core adapter files (`query/resources/inputs.rs`, `query/runtime/driver.rs`, `query/runtime.rs`, `tests/graph_compiled_context.rs`). Root separately accepted them in `/tmp/ze-126-core-adapters/root-review.md`. Reading their existing consumer/accounting loops to explain observations is not a new self-review approval.

## Concrete blocker: unpolled copied-parameter UTF-8 scan

`lowering/owned.rs:67-68` calls `std::str::from_utf8` on a complete copied range without a checkpoint. `lowering/parameters.rs:144-145` uses it for every copied String. Source text is capped at 65,536 bytes, but a parameter String is separately permitted up to the query envelope. Copying polls byte by byte; this later full revalidation does not.

The independent probe `copied_parameter_can_exceed_a_single_utf8_checkpoint_chunk` supplies `"é".repeat(65537)` to `RETURN $text AS copied`. It passes, verifies distinct backing and exact copied contents, and records **131,074 parameter bytes**, **597,070 peak charged bytes**. This proves the relevant input is admitted rather than relying on a nominal maximum. The source call then performs a whole-range UTF-8 validation exceeding the existing 64 KiB byte-work interval. This is a bounded-polling defect, not a demonstrated memory-safety flaw.

The exact same unpolled UTF-8 concern was corrected and independently reviewed in `tasks/evidence/ze-49/runtime.md:110-119`; its controlled row byte-copy case spans 131,073 bytes. A correction must preserve valid UTF-8, actual ownership, immutable lifetimes, and bounded polling. Root has approved pursuing a safe public core helper with a narrowly reviewed internal invariant; outer compiler unsafe denial must remain. Raising the polling interval or adding an unchecked frontend conversion is not this review's recommendation.

Probe source: `/tmp/ze-126-resource-review/crates/zeppelin-embed-cypher/tests/resource_review.rs`. Raw result: `/tmp/ze-126-resource-review/probe.log`.

## Safe inspected boundaries

- `ReadContext` is sealed to actual `ValueContext` and `RuntimeContext`; the callback receives the same mutable C. No independent control, lease, work budget, or reset is constructed. RuntimeContext checks exact QueryMemory identity. The ValueContext route remains symbolic preparation and does not claim an admitted view.
- PreparationControl borrows the original retained adapter and QueryControl with their original lifetime. Every such checkpoint checks retained-view activity first, then caller cancellation/deadline. The final callback-return check discards even an already built successful output if close wins there.
- Typed ResourceError mapping preserves ReadCancelled, Cancelled, Timeout, and WorkLimit. Native PlanError is preserved except explicit limit classification. The cumulative validator and runtime steps use the original ValueContext.
- Compiler capacities remain conservatively reserved while compiler allocations are alive. Lowerer buffers use actual charged QueryArena backing; growth reserves replacement capacity while old capacity remains charged, then copies with checks before releasing old backing. No borrowed caller length is treated as ownership credit.
- Parameter names, strings, source and metadata are copied. Nested values freeze deepest-first across 17 distinct arenas, so parents borrow immutable child owners without self-reference or lifetime widening. All relevant arenas and authenticated facts remain alive across the HRTB consumer. Owner inventory corresponds to those actual arenas; a facts token removes only the actual facts duplicate at later admission.
- CompileLimits enforce source 64 KiB, 8,192 tokens, 4,096 AST nodes, 64 depth and 256 parameters/columns. Canonical `cypher.md` and `qualification.md` explicitly retain these additional compiler guards. Missing ValueContext::step on raw frontend compiler walks alone is **not** a defect: their control checks and declared compiler bounds are separate from the actual validator/execution value-work account. No invented expression work is charged for mere compiler traversal.

## Independent focused verification

Command in owned scratch:

```text
cargo nextest run -p zeppelin-embed-cypher --test resource_review --test runtime_lowering --success-output final
```

All four supplied final runtime tests pass, plus the large-parameter admission probe. One exploratory precedence assertion intentionally differs from observed behavior (described below), so the aggregate command exits 100: **5 pass / 1 fail**. This is not represented as a fully green command.

| Runtime test | Observed scope |
| --- | --- |
| `compiled_read_drains_through_the_same_runtime_context_and_real_owners` | Actual retained SnapshotLease/View descriptor, same RuntimeContext pointer, real QueryInputs and RuntimePlan admission, existing driver drain/completion. Prior value work 11, prepared 301, completed 307; exact execution increment +6. Facts backing 4,144 bytes; credited admission delta 272. Original counters remain cumulative; near-limit execution fails with no completion and no reset. |
| `compiled_read_parser_checkpoint_preserves_close_before_simultaneous_cancel` | Real retained close and simultaneous caller cancellation at parser checkpoint; ReadCancelled wins, consumer never runs, reservations return to baseline. |
| `compiled_read_final_check_discards_actual_completed_output_and_all_owners` | Real drain completes to charged output, then close+cancel fires before compiler handoff; output is dropped once and all temporary charges return to baseline. |
| `compiled_read_validation_reports_original_cumulative_work_exhaustion` | Original value account seeded to its limit causes typed WorkLimit before the consumer, without reset or leaked reservation. |

The owner's earlier raw run reported 11→294→300. Both are valid observations, not interchangeable exact measurements. Source verifies that address-key QueryInputs heapsort sifts and plan-backing binary searches increment real work on address-dependent branches. Absolute prepared work therefore varies with allocation layout; invariant prior work 11 and actual execution delta +6 are what the test asserts.

Recompiled the positive and forbidden-escape probes against this scratch's final generic-interface rlibs: owned column-count result exits **0**; returning LoweredRead exits **1**, with separate plan/facts lifetime errors. Exact commands/output are `/tmp/ze-126-resource-review/lifetime-proof.json`, `positive.log`, and `escape.log`. These are current frozen-interface results, not reuse of the initial interface's old proof.

## Non-blocking observation and exclusions

A cancelled ValueContext with QueryMemory filled to exactly MAX_QUERY_BYTES gets `Resource(Memory)` while acquiring the initial compiler reservation, before its first checkpoint. The exploratory test expected Cancelled and fails accordingly. No accepted contract found requires cancellation to supersede every earlier memory/argument error; the accepted close-before-caller-cancel guarantee applies when that checkpoint runs. Root agreed this is not a blocker without a stronger contract. No guard or error policy was changed for the experiment.

This is a bounded ownership/control review, not complete Cypher semantic acceptance, general graph execution, public admission, native binding parity, TCK execution, full-profile qualification, allocation instrumentation coverage, or a broad adversarial campaign. The runtime tracer consumes a scalar literal via an actual controlled producer; it does not certify general evaluator/operator composition. Those original gates remain assigned to their owning tickets. Broad nonessential qualification remains ZE-118.


## Frozen UTF-8 correction: independently verified, blocker resolved

Correction `/tmp/ze-126-additions-review-1/inventory.json` SHA-256 `c167ae5b590a65894bbb31150e670c8e7ac22f7202cf8e6666a068bfdc73d49a`: all **16/16** frozen paths match. Independent correction scratch is `/tmp/ze-126-utf8-review`, with prior assembly plus this immutable delta recorded in `utf8-review-assembly.json`. Completed-edge semantics in the same delta belong to the other reviewer; they are included only as immutable build dependencies here.

`property_graph/utf8.rs::checked_utf8` extracts the existing catalog algorithm exactly: call the original callback before each at-most-65,536-byte validation window; retain an incomplete trailing codepoint for the next window; reject malformed or terminal truncated input; return a borrowed string without allocation. Every successful byte belongs to a completely validated span. The single documented internal unchecked conversion uses those immutable bytes only and does no second scan. The safe generic public helper retains caller error E as `Utf8CheckError::Control(E)`. The catalog wrapper maps Invalid back to Malformed and passes the identical callback/error through, including its unchanged empty-input no-poll behavior.

Frontend `owned::text` now passes its original control callback to that safe helper. Every production text call site supplies the original close-first `self.control`. The parameter String freeze path therefore polls while validating copied strings. Malformed bytes stay a BindingInvariant error; exact ResourceError control variants pass through. The outer crate still denies unsafe code; no frontend unsafe conversion, lease, memory owner, or budget was added. The public HRTB input/output lifetime shape is unchanged.

Independent focused terminal GREEN: **3 core UTF-8 tests + 1 catalog UTF-8 regression + 7 frontend tests = 11 tests**. The frontend set consists of the actual copied-text polling test, original 131,074-byte admission/copy probe, 131,077-byte split-four-byte parameter test, and all four runtime tests. Core tests exercise all three possible split positions within a four-byte codepoint, invalid/truncated/overlong/surrogate/out-of-range data around boundaries, exact control failures at windows 1/2/3, and empty input. No broad suite ran.

Exact independent commands (all in correction scratch with `CARGO_TARGET_DIR=/tmp/ze-126-resource-review/target`):

```text
cargo nextest run -p zeppelin-embed --test graph_checked_utf8 --success-output final
cargo nextest run -p zeppelin-embed --test graph_catalog -E 'test(catalog_utf8_spanning_work_chunks_is_preserved_and_invalid_continuations_fail)' --success-output final
cargo nextest run -p zeppelin-embed-cypher --lib --test read_lowering --test resource_review --test runtime_lowering -E 'test(copied_text_preserves_four_byte_boundary_and_polls_every_validation_window) | test(read_lowering_large_copied_parameter_preserves_split_four_byte_utf8) | test(copied_parameter_can_exceed_a_single_utf8_checkpoint_chunk) | binary(runtime_lowering)' --success-output final
```

Raw logs: `core-utf8.log`, `catalog-utf8-green.log`, `frontend-utf8-green.log`. Corrected-source raw runtime observation is prior **11**, prepared **299**, completed **305** (still exact execution increment +6), facts **4,144**, admission delta **272**. The original large parameter remains **131,074 bytes**, peak **597,070**. These are this run's measurements; preparation address-order qualification remains above.

CAN FIRE: in this isolated scratch only, bypassed the actual frontend text callback with a no-op while retaining the helper algorithm. `copied_text_preserves_four_byte_boundary_and_polls_every_validation_window` failed at runtime with **polls 0 versus 3**, nextest **100**. Restored the exact frozen source; the named test then passed, nextest **0**. `wire-mutant.json`, `wire-mutant-red.log`, and `wire-restored-green.log` retain commands/results. Restored owned.rs SHA-256 is `1ac3dc5093dda21e1cd01ee7b01ac54058178f95d7c3a1ff1022e8241d251b52`; final all-16 inventory hash audit still matches.

Recompiled positive and LoweredRead-escape probes once more against the correction build: **0 / 1**, with both forbidden lifetime escapes rejected, recorded in `lifetime-after-utf8.json`. The prior resource ownership/control findings therefore remain cleared on the corrected source. This disposition certifies neither my four excluded core adapter files nor complete lowering/TCK/general execution acceptance.
