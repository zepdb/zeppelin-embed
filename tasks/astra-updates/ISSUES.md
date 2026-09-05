# Development issues

Execution issues are recorded below with actual evidence. Issues 001, 002 and 005 are resolved; 003 and 004 remain nonblocking follow-ups.

Capture, classify, log and move on unless the issue blocks the current plan,
safe/correct results, or a trustworthy required benchmark. Nonblocking issues
are follow-up work, not an implicit scope expansion.

## Issue index

| ID | Plan / commit | Summary | Severity | Blocks what? | Status | Evidence / follow-up |
| --- | --- | --- | --- | --- | --- | --- |

Use IDs ASTRA-ISSUE-001 onward. Suggested statuses: logged, reproduction
unconfirmed, fixing-blocker, externally-blocked, resolved, deferred-follow-up.
Use "None for current plan/benchmark" explicitly for nonblocking findings.

## Entry format

Copy a short entry below when an issue is actually observed:

    ### ASTRA-ISSUE-NNN: concrete symptom
    Plan and observed commit:
    Location:
    Expected:
    Actual:
    Reproduce: exact command, seed, input/epoch/hash, or original log path
    Evidence:
    Severity:
    Blocking: yes/no; exact implementation or benchmark requirement affected
    Current action: minimal prerequisite fix / log and continue / external blocker
    Follow-up: proposed scope and why it belongs outside the current plan
    Status:
    Resolution commit and focused test, if any:

## Triage rule

A new regression in this plan's scores, identity, corruption handling, ownership
or required tests is a blocker even if a benchmark becomes faster. A baseline
unrelated failure is logged once with its prior-state evidence and does not
trigger broad repair work. If an issue's impact on the benchmark is uncertain,
perform one bounded investigation sufficient to classify that dependency, then
record remaining uncertainty. Never label an affected comparison valid by
assuming an unresolved correctness failure is harmless.

At final handoff list remaining nonblockers concisely and identify true blockers
separately. Do not postpone landing otherwise verified commits to clean the
nonblocking backlog.


### ASTRA-ISSUE-001: lexical worker borrows an expired window argument
Plan and observed commit: 00, baseline 0a8caf5.
Location: lifecycle/mod.rs submit_lexical_leg and lifecycle/pool.rs LexicalWorker::submit.
Expected: a full-width (401 requested) round returns all 200 matching rows.
Actual: standalone lexical returns 200; the hybrid final round reports 41 evaluated rows.
Reproduce: cargo test --offline -p zeppelin-embed --test hybrid_bounded astra_00_all_hybrid_rounds_contribute_work -- --exact --nocapture (original probe; now narrowed to astra_00_lexical_full_width_survives_worker_handoff).
Evidence: tasks/evidence/astra-00-raw/rounds-fixture.log. submit erases the closure lifetime but returns a PendingLexical with no lifetime connection. The closure borrows the submit_lexical_leg call's by-value bound, which expires before wait.
Severity: memory safety / incorrect retrieval.
Blocking: yes; safe execution and trustworthy counters and benchmarks.
Current action: minimal prerequisite: contain scoped jobs within a synchronous scope that always joins, including unwind. Preserve concurrent vector execution and typed panic handling.
Status: resolved in 12ee589; astra_00_lexical_window_survives_worker_handoff RED to GREEN.

### ASTRA-ISSUE-002: I49 models the previous fusion producer policy
Plan and observed commit: 00 prerequisite, 0a8caf5 and scoped worker patch.
Location: adversarial-oracle/src/hybrid_fusion.rs expected_case starts at k and models full-list extremes; current store producers start at max(50, 5*k) with bounded extremes.
Reproduce: the original directed I49 probe using build_hybrid_episode(11); repeated with a 41-row seed 14 control.
Evidence: handoff-directed-before.log, handoff-directed-after.log, handoff-directed-small-before.log, handoff-directed-small-after.log under tasks/evidence/astra-00-raw. All reject Vector clean retry versus independent expected fusion. These failed experiments are preserved.
Impact: I49's full-score/report comparison remains unqualified. This observation does not establish whether all score differences are oracle-only; plans 01-03 own coverage, cross-scoring and normalization.
Blocking: none for the worker ownership prerequisite; blocks full fusion qualification.
Current action: log and continue. The final directed ownership test keeps seed 11, checks literal panic types, no partial result, both joins, and same-seed clean/retry equality; a missing-join plant must fail. No existing independent oracle was changed.
Status: resolved in 1f12858 at the after-03 checkpoint; see the resolution below.

### ASTRA-ISSUE-003: existing core Clippy warnings block -D warnings
Plan and observed commit: 00 prerequisite on 0a8caf5.
Reproduce: cargo clippy --offline -p zeppelin-embed --lib --no-deps -- -D warnings
Evidence: tasks/evidence/astra-00-raw/handoff-clippy.log, exit 101. Unused arguments, unit arguments, drop_non_drop, needless lifetimes and mismatched lifetimes are on unchanged lines in kernels, lifecycle vector execution, planner and segment reader. None point to the scoped-worker changes.
Impact: strict core lint qualification is not GREEN.
Blocking: none for current plan/benchmark.
Current action: log and continue; no unrelated cleanup or lint suppressions.
Status: deferred-follow-up.

### ASTRA-ISSUE-004: clocks-disabled lexical latency regresses with unchanged work
Plan and observed commit: plan 00 working diff d34a4bd5f0ef7376d210a91434c3f7e37f285b0fa3cf011cd6ff21591aadd678 on 12ee589.
Expected: report instrumentation/counter overhead and investigate p95 increases over 5%.
Actual: initial lexical off/baseline p95 +6.19%; bounded same-input repeat +7.87% (3.022208 to 3.260166 ms). Timing-enabled p95 stays near baseline. All exact ranks/score bits and deterministic work match.
Evidence: tasks/evidence/astra-00-scan-screen and astra-00-lexical-recheck; all 36 cells exit 0, raw samples and process receipts preserved.
Impact: measured warm lexical latency cost, mostly inside core retrieval (off/on median 1.1024165/1.004896 ms). Underlying cause unresolved; no CPU/code-generation explanation is asserted.
Blocking: none for required correctness/observability implementation or truthful screen; no speedup is claimed.
Current action: bounded investigation completed; retain required fix with negative measurement.
Follow-up: subsequent lexical/query performance plans should track this same frozen control and report whether the cost moves.
Status: deferred-follow-up.

### ASTRA-ISSUE-005: evicted lexical assembly loses its charge while owned

- Found during plan 03, parent aa9c409, `CachedLexicalAssembly::_memory`.
- Reproduction: `cargo test --offline -p zeppelin-embed --lib
  astra_03_lexical_assembly_charge_survives_cache_eviction`, exit 101, one test.
  A public lexical query builds the cache; two query-style Arc owners retain
  its data; deterministic eviction releases 548 cache bytes (548 -> 0) while
  the live assembly still contains its document. Raw `astra-03-raw/assembly-charge-red.log`.
- Severity: incorrect resident-budget accounting. Blocks new plan-03 retention
  of lexical assembly across producer join and candidate cross-scoring.
- Minimal prerequisite: move AccountedCounter into the Arc-owned assembly,
  reserve before publication, and retain the old charge if any owner survives
  eviction. Replacement may correctly fail budget admission while old data lives.
- Focused GREEN: same retained-owner test plus final-owner exact-zero assertion;
  `assembly-charge-{green,final-green}.log`, exit 0, 1 executed each.
  Final charge 572 bytes retained through eviction/two owners, then exactly 0.
  Resolved in prerequisite 60a0e9d (+24 bytes correctly charged owner metadata). No broader cache
  redesign or plan-16 incremental assembly work included.

### ASTRA-ISSUE-002 resolution at the after-03 checkpoint

Resolved in 1f12858. The independent oracle now accepts explicit Store policy v1
facts (fixed zero anchors, validated vector enclosure, complete cross-scores,
actual 50/5k window replay) while retaining its pure min-max/RRF input variant.
Seed-11 I45-I49 checks, including both panic joins and exact same-seed retries,
pass; directed missing-score/policy/round plants and an independent literal
boundary-floor plant fire. See astra-03 evidence and regression-receipts.json.

### ASTRA-ISSUE-006: serial bounded exact scan retains two stress regressions

- Plan 04, matched final release AB/BA/AB screen, six synthetic cells/process,
  20 warmups/64 samples, independent scalar ID/score-bit and byte oracles.
- N=1,024/k=10 mixed scores: p95 25.375 -> 27.709 us (+9.20%, +2.334 us).
  N=8,192/k=10 all equal scores: 209.958 -> 275.708 us (+31.32%, +65.750 us).
  All returned IDs, bits and vector work match the reference.
- Evidence: `tasks/evidence/astra-04-raw/core-runs.json`, `core-summary.json`.
  Initial near-N regressions were resolved by bounded 2k unordered selection;
  their original measurements are preserved, not hidden.
- Nonblocking for required bounded storage: N=60,000/k=10 p95 improves 48.46%,
  native FiQA Exact dense 9.37%, hybrid 9.18%, with identical rankings/work.
  Ordinary scratch is O(k); all-boundary-tied inputs legitimately require O(N).
- Follow-up: track the same small/tied cases through plan 05. Do not infer an
  unmeasured kernel or allocator cause. No broader collector cleanup requested.
- Status: deferred-follow-up; explicit negative measurements retained.


### ASTRA-ISSUE-007: close can miss the final snapshot release

- Discovered during plan 05's expanded cancel/deadline/close worker barriers.
  `focused-final.log` executed five passing tests, then the close barrier
  hung. `/usr/bin/sample` captured close parked in `drain_readers` while
  all 12 query workers were idle and the query driver had already joined.
  The task-owned test process was terminated after retaining that evidence.
- Cause: SnapshotLease notified before its Arc field was dropped. A close
  waiter could observe the extra reference and sleep before the decrement,
  with no later notification. Complete vector/lexical admission also held a
  raw Arc beyond its last partition lease without notifying on release.
- Deterministic RED: `cargo test --offline -p zeppelin-embed --lib
  astra_05_snapshot_release_notifies_after_ownership_drops -- --nocapture`,
  exit 101, 1 executed. At notification the reference count was 2, expected
  only the draining owner's 1. `reader-release-red.log` retains the failure.
- Blocking: safe and terminating close is required for plan 05. Minimal
  prerequisite gives public leases and complete admissions an owning wrapper
  whose fields drop the snapshot before a separate release guard notifies.
  The independent synchronization allocation is charged before allocation
  and remains owned through its final guard. No unsafe code, polling repair,
  persisted format or query ranking change.
- GREEN: same unit plus final-admission notification, 1 passed; existing
  close_is_idempotent and close_unmaps_all_segments, 1 each passed. The
  uncommitted plan-05 six-test set also passes, including all 12 barrier cases.
  Raw logs under tasks/evidence/astra-05-raw. Resolved in prerequisite cf312af.


### ASTRA-ISSUE-008: shared Cargo target reused an isolated-worktree library

- Plan 05, `core-capacity-red-build.jsonl`: main's new test compile reported
  15 missing symbols and pointed to the clean71 isolated worktree's older core
  source. Cargo had reused the rlib overwritten by that worktree's build in
  the shared target directory. This was a compilation failure, not a behavioral
  RED or a product defect. No test or timing cell executed from this build.
- A stale cross-worktree library invalidates build provenance. Main crate roots
  are touched to force their own source rebuild, with build receipts retained.
  Follow-up isolated builds must use a distinct CARGO_TARGET_DIR. The subagent
  has stopped building and its exact tested binaries are already preserved.
- Previously preserved plan-05 core binary executes the new worker/receipt
  paths and passed every capacity cell against the independent full-corpus
  oracle. Its immutable SHA is in preserved-binary-hashes.json. It is not
  replaced or retrospectively relabeled. Native confirmation uses a fresh
  forced build after the selected capacity change.
- Status: resolved for the current run by verified main-source rebuild;
  no source cleanup required. Distinct target directories required next time.


### ASTRA-ISSUE-009: observation-only fault controller contends per row

- Initial Step 05 capacity/crossover screens found increasing parallel latency
  on short vectors. `VectorFaultController::after_eligible_row` acquired its
  mutex for every row even with no fault armed, serializing observations.
- Blocking for trustworthy capacity selection. Initial pool-summary.json,
  pool-shared-four/pool-summary.json and crossover-summary.json retain valid
  identity/work evidence but their timings are superseded; they cannot prove
  hardware bandwidth contention or select four versus eight workers.
- The immutable fault kind is cached on construction. Observation-only calls
  bypass the inapplicable hook; armed CancelAfterRows retains synchronization
  and receipts. Fresh uncontended 27-cell capacity and six-process crossover
  screens completed. Seven focused tests and armed cancellation are GREEN.
- Status: resolved in b743113. A separate serial regression was subsequently
  attributed to generated collector calls and resolved as issue 010. No
  uncontended eight-versus-four comparison is claimed.

### ASTRA-ISSUE-010: parallel collector reuse outlines serial per-row updates

- Found while qualifying Step 05 against fcc5999: six serial controls retained
  exact IDs/bits/work but several p95 cells regressed 6-9%. This blocked the
  predeclared serial tolerance despite the separate native parallel speedup.
- Matched temporary stage profiling (`astra-05-raw/stage-profile/RESULT.md`)
  localized the delta to scoring. Before/after generated assembly in
  `astra-05-raw/scorer-assembly/` showed a collector function call for every
  row after parallel scans introduced additional collector consumers.
- Inlining ExactTopK::try_push fixed the unordered frontier controls. The
  compiler then outlined BoundedTopK::try_push, and after that wrapper was
  inlined it outlined push_with_reserve. These intermediate negative/partial
  results and generated assembly are preserved in inline-collector/ and
  inline-heap/. No score, tie, allocation, failure or cancellation logic changed.
- Keeping that three-method update chain inline restores baseline code
  generation. `inline-heap-body/scorer-calls.json` proves no such calls remain
  in any of three scorer specializations. Its six AB/BA/AB normal-release
  serial controls all meet max(5%,1 us); the largest p95 increase is +0.84%.
  Seven focused tests and the armed cancellation regression are GREEN.
- Status: resolved in b743113. Final-code native confirmation also passed:
  Exact dense p95 -64.77%, hybrid -63.39%; all serial controls within tolerance.

### ASTRA-ISSUE-011: new scoped callback finalized kernel publication too early

- Found during Step07 working-tree audit on d66b161. The public dense callback
  could copy a row, cancel the query and return Cancelled, while the forced
  backend's scoring receipt still recorded result_published=true. This is a
  Step07 integration defect, not a pre-existing main behavior or a score error.
- Exact reproduction: cargo test -p zeppelin-embed --test query_lifecycle
  astra_07_cancelled_materialization_does_not_report_published_kernel_results
  -- --exact --nocapture. publication-red.log: one executed assertion failed,
  observed true, expected false. Seed/case 11, forced Scalar, one active row.
- The continuation now finishes inside the owning public scoring operation,
  before kernel/vector receipt finalization. publication-green.log passes the
  cancelled case and identical seed/backend clean control with nonzero work.
- Severity: inaccurate failure/publication evidence. Blocked Step07's final
  fault-contract audit; resolved in 1f9f8ad with directed GREEN and a refreshed
  final native comparison.
  The earlier native screen uses no forced controller and retains its hashes;
  final binary/source comparison will determine whether timing needs refresh.

### ASTRA-ISSUE-012: bundle exporter silently selected only the query tokenizer

- Found while validating Step 08's source contract. The v1 bundle has one
  tokenizer table, but bake() serialized the query table without checking that
  the document source had the same table and normalization contract.
- Reproduction: python3 tools/ze-model/test_tokenizer.py
  WordPieceExport.test_astra_08_bundle_refuses_distinct_tokenizer_contracts.
  Two valid small BERT vocabularies differing at one normal token reached
  serialization (export-pair-red.log; one test, exit 1). Model loading is stubbed
  in this offline exporter test; actual encode_tokenizer and bake validation run.
- Impact: a newly exported pair could embed documents with another tokenizer.
  This blocks trustworthy export for Step 08's supported contract. Both actual
  pinned source tokenizers are identical after validated encoding (445186 bytes).
- Resolution: minimal pre-serialization equality check; differing contracts
  fail with ValueError. Four exporter tests pass, including source/config
  consistency controls (export-settings-green.log).
  Fixed in Step 08 commit b496446; no format or separate-tokenizer
  runtime was added, and no existing bundle/store bytes changed.

### ASTRA-ISSUE-013: experimental Unigram tokenization differs on a full-corpus input

- Found during the separately authorized clean71 4-bit comparison in
  /private/tmp/ze-clean71-smoke-bmoljupi/worktree (detached cf312af plus the
  pre-existing model/tokenizer adapters). Main's LEAF/Arctic WordPiece control
  is not affected by this experimental Unigram mismatch.
- The preserved source-token oracle fails on fixture document 2301, containing
  repeated em dashes. The original unbounded experimental binary reproduces
  the same incorrect segmentation; the later maximum-token-length bound is
  not the cause. Raw evidence:
  /private/tmp/ze-clean71-4bit-mscs_8rg/bounded/clean71-full-tokens-v2.log and
  bounded/case2301-unbounded.log. Full command receipts are retained by the
  model experiment in that artifact root.
- The adapter accumulates f32 path scores over joined whitespace words; the
  pinned source declares per-word WhitespaceSplit/Metaspace and f64 path scores.
  The experimental correction now uses source f64 vocabulary/path scores,
  independent pretokenizer pieces and pre-normalization added-token matching.
  New layout3 bundles preserve all original tower tensor digests and store
  f64 tokenizer scores. Evaluation is versioned with ;ze-text-unigram=2.
  No main integration is authorized by this repair.
- The exhaustive end-boundary loop also hashes impossible substrings longer
  than every vocabulary token. Its exact vocabulary-byte bound reduced the
  same 256-document/64-query token check from 63.59 seconds to 0.387 seconds,
  with unchanged smoke IDs. This speed observation alone is not correctness
  qualification for other inputs.
- Severity: wrong source token IDs can change document embeddings and model
  quality conclusions. The corrected final runtime passes exact source IDs
  for all 58,980 chunks and 648 queries, both new-pair source-vector suites,
  added-special/literal-marker cases, and three focused tokenizer unit tests.
  Both actual old Unigram smoke stores fail with EpochMismatch, preserving
  every file's bytes, size and mtime. Receipts: faithful/validation-v2-complete.json,
  faithful/old-{fp32,int4}-epoch-refusal.log and faithful/tokenizer-unit-tests.log
  under the artifact root above. This clears the correctness blocker for fresh
  measured indexes; it does not establish retrieval quality or performance.
  Does not block independent main Step09 development. The interrupted full
  FP32 ingest is preserved and explicitly unusable as a completed comparison.
  The final full-FiQA comparison is now complete: all 65 benchmark processes
  exited 0, including 36 independent full-query processes covering all 648
  judged queries each. The parent independently verified the original corpus
  text/qrels, query IDs, separate indexes and packed-operation counts.
  Final report: /private/tmp/ze-clean71-4bit-mscs_8rg/faithful/REPORT.md;
  independent receipt: faithful/parent-audit.json. The tokenizer blocker is
  resolved in the experimental runtime. The measured 4-bit quality loss is
  reported as a quantization tradeoff, not a remaining tokenizer failure.

### ASTRA-ISSUE-014: close retained the immutable lexical assembly cache

- Step 12's refused-vocabulary test returned a typed temporary-budget error and
  correctly left its new dictionary slot empty, but after successful close the
  existing assembly remained charged: 490 cache bytes versus the required zero.
  Evidence: `tasks/evidence/astra-12-raw/focused-0.log`, four focused tests with
  this one failing. Inspection confirms the prior close path dropped snapshot,
  workers and active state without evicting `lexical_index_cache.entry`.
- This blocks Step 12's explicit cache release requirement. The minimal repair
  drops the cache owner after reader/worker drain, both in explicit close and
  best-effort Drop. Retained query dictionaries/assemblies keep their own Arc
  reservations. No persisted data, publication ordering or query result changes.
- `astra_12_refused_vocabulary_is_not_published` and
  `astra_12_vocabulary_charge_survives_eviction_and_close` pass after the repair;
  `focused-final-0.log` records terminal GREEN. In-flight close integration is
  checked separately. The repair is included in Step 12's cache ownership scope.

### ASTRA-ISSUE-015: vocabulary rebuild cost after every active mutation

- Step 12 final matched core screen preserves every expansion, score, snippet
  and posting count, but its instrumented post-update prefix p95 is
  528.000 -> 827.250 us (+56.68%). Raw:
  `tasks/evidence/astra-12-core-linear/aggregate.json` and six process outputs.
  Every update rebuilds once under the new exact input identity. These clocks
  include armed work observation; they are rebuild diagnostics, not production
  cold-query qualification. Warm unarmed p95 improves 447 -> 14.125 us.
- The completed investigation replaced per-term allocations with three owned
  buffers and reduced grouping checks from 133,617 to <=20,000 for 10k terms
  (literal RED to GREEN). Earlier slower variants remain preserved. Changed
  inputs still rebuild the complete field-aware dictionary; this is unsuitable
  for a workload with only one prefix query per mutation.
- Nonblocking performance follow-up: consider sharing/merging immutable
  per-segment vocabulary contributions alongside the later assembly reuse work.
  Do not change visibility or skip required input invalidation to hide the cost.
  Step 12 remains retained for its demonstrated warm work/latency reduction,
  with this negative result explicit. No correctness or native-control failure.

- Step 13 also quantifies its added eager length-index cost on a separate
  4097-term control: cache +65,576 bytes, instrumented first prefix query
  87.792 -> 129.917 us and update p95 256.115 -> 298.958 us. Warm prefix p50
  stays 4.459 us. This is an explicit nonblocking tradeoff of the retained
  fuzzy optimization (Store p95 -48.62%, fuzzy hybrid -39.22%), not a failed
  correctness gate. Raw: `tasks/evidence/astra-13-measured/aggregate.json`.
  If prefix-only/update-heavy measurements justify it later, consider lazy
  length-index ownership without weakening exact vocabulary invalidation.

- Step 14's lazy phonetic map adds 65,664 bytes for 4,099 terms and increases
  instrumented rare first use 438.000 -> 550.083 us, rare update p95
  584.052 -> 665.281 us and hybrid update p95 616.771 -> 716.594 us.
  Warm rare/hybrid p95 improves 97.87%/85.81%, so the conditional map is retained.
  The 2,048-term collision workload has no end-to-end win (p95 +1.18%).
  Raw: tasks/evidence/astra-14-measured/aggregate.json. This extends the same
  nonblocking rebuild/candidate-cost tradeoff; no unsafe cache reuse or hidden
  expansion cap is introduced to manufacture a win.

### ASTRA-ISSUE-016: existing workspace formatting differences

- Observed at parent bfc2945 with `cargo fmt --all -- --check` (exit 1).
  Only unchanged quant/tests.rs, tests/prune_contracts.rs,
  tests/adversarial_tests.rs and tests/graph_refinement.rs differ from rustfmt.
  Step 14's eleven changed files pass scoped rustfmt and git diff --check.
- Raw: tasks/evidence/astra-14-raw/final-format.log and final-checks.json.
  This is a nonblocking formatting backlog, with no behavioral consequence.
  Preserve unrelated files; handle cleanup separately if requested.

### ASTRA-ISSUE-017: API benchmark enables hot fault observers

- The regular benchmark crate depends on core with `test-support`. On the
  clean71 cf312af runtime, every four-row Bit4 batch takes two global observer
  mutexes and allocates a score-bit vector, even without a controller. Main
  retains these hooks too. This materially distorts parallel scan/native API
  timings. Matched same-source feature-only controls measure core scan p50
  2.266 -> 0.130 ms; dense API 3.573 -> 0.966 ms. Raw/evidence:
  `/private/tmp/ze-query-investigate-vci05npf/attribution-summary.json` and
  `tasks/evidence/query-api-scan-embedding.md`.
- This blocks interpreting those instrumented wall times as normal application
  performance. Earlier exact-output/work evidence and explicitly instrumented
  before/after comparisons remain historical evidence; they do not establish
  the same relative speedup in normal-feature applications. No old result is
  silently rebaselined. Reconfirm retained policies using normal feature graphs
  at the next applicable benchmark checkpoint.
- Resolution for query-budget timing: a separate workspace/manifest excludes
  `test-support` and leaves existing fault hooks and integrity checks intact.
  General measurement correction; no FiQA-specific runtime tuning. Future
  benchmark binaries must record their feature graph and separate work-counter
  builds from production-feature latency controls.

### ASTRA-ISSUE-018: stats rejects a retained old active generation

- During Step 16's deterministic old/new query lifetime test, `Store::stats`
  fails with `Statistics { component: "active segment accounting" }` after
  active append while an older admitted query still owns the previous active
  buffers. `stats_while_open` compares all charged active bytes against only
  the current active segment. That code is unchanged from parent 7d0f9ef.
  Evidence: `tasks/evidence/astra-16-raw/lifetime-diagnosis.log`, one failed
  test; the initial barrier probe required termination because assertion failure
  did not release its peer, then a release-on-unwind guard exposed the error.
- Nonblocking for contribution-cache correctness: the internal accounting
  audit still covers both generations. The cache lifetime probe uses those
  exact counters directly. This prevents relying on public stats during this
  overlap; it does not justify dropping old-generation reservations.
- Follow-up: make stats distinguish current active ownership from retired
  active generations retained by queries, with independent lifetime/accounting
  tests. No unrelated stats repair is included in Step 16.
