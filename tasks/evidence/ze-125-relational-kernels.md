# ZE-125: bounded relational kernels and eligible sets

The production component now implements filter, projection and scope changes,
offset/limit, DISTINCT, stable sorting, grouped/global count and collect, and
immutable same-view eligible node sets. Composed operators retain real charged
buffers across pull batches inside one owned execution view. The stateless and
retained-chain entry points use the same driver drain, eager preparation,
completion and final-check implementation.

This is component acceptance under the owner-approved
[ZE-122 parallel contract](ze-122/parallel-contracts.md). Kernels consume typed,
already evaluated columns in an explicit ordered `SlotId` schema. Projection
copies/renames columns; filter admits true and discards false/null, rejecting
other values. Expression evaluation and native entity/property reads must be
supplied by ZE-50/51. Reading a column does not increment an expression-work
counter. These tests do not qualify native admission, structured public query
execution, Cypher/TCK execution, search ranking, persistence or recovery.

## Source and ownership

- Worktree: `zeppelin-embed-wt-ze-125`, branch
  `codex/ze-125-relational-kernels`, base
  `7028a42f2f4898bb7aa729dc935bd51b4033f2ba`.
- Frozen source inventory: [source-hashes.json](ze-125/source-hashes.json).
  Snapshot identity and host: [verification.json](ze-125/verification.json).
  Raw evidence checksums: [artifact-hashes.json](ze-125/artifact-hashes.json).
- Twenty owned source/test paths; no changes to plan/mod.rs or `NodeFacts`.
  `Schema::verify` uses the existing width/slot accessors. ZE-123 retains sole
  ownership of any new facts accessor.
- Inherited AGENTS.md, .agents/, CONTEXT.md and tracker entries and the prior
  ZE-120 worktree remain preserved outside this commit.

`PullOperator<'v, 'm, 'g>` binds both `pull` and `prepare_search` to the execution
view and memory lifetimes. `OperatorFactory` has a GAT operator type and constructs
the chain fallibly inside the owned view. Construction is charged preparation
before visible rows. Existing `execute` retains its higher-ranked stateless
contract; `execute_factory` invokes the same drain implementation. The lifetime
rejection proves a fresh view cannot escape into a static factory field. No
unsafe lifetime widening, duplicated budget or second execution semantics were
introduced.

`Rows` owns its schema, actual flat row/variable storage and order arena.
`MapRows` owns bounded input and projection/schema/control storage.
`BlockingRows` drains its real child only after eager preparation, preserves the
source barrier even under LIMIT 0, and emits bounded batches after materializing
one kernel. Old/input/output/scratch owners retain their authentic simultaneous
reservations. Consuming kernel failure exposes no successful partial result.
The scheduling limit remains 256 rows and the driver's completed-row limit
remains 65,536; a separately charged intermediate store can exceed that completed
limit. The focused test sorts 70,000 intermediate rows without changing a cap.

Sorting uses stable cancellable merge sorting. Hash grouping checks equality on
collisions using existing query value equivalence, including null, NaN, exact
mixed numeric values beyond 2^53, and recursive lists. Count/collect skip null,
DISTINCT preserves first occurrence, and collect preserves upstream order.
Empty global aggregation returns 0/[]; empty grouped aggregation has no rows.
Collected entities use packed full NodeIds and nested list depth/cardinality
are rechecked during copying.

`EligibleNodeSet` owns one packed NodeId arena, sorts and deduplicates full
128-bit identities, and validates the actual QueryView identity. Hash scratch
is separately charged and released before the immutable result returns.
`AllIndexed` is distinct from an empty set. The 524,288 cap applies to each
materialized set, while each supplied list has its own total-element bound;
examined duplicate inputs are cumulative work, not a new cardinality cap.
The test examines 524,289 duplicate inputs and retains one ID. Hashing mixes all
16 identity bytes with the already permitted xxhash implementation.

Completion counter ownership is unchanged and was coordinated with ZE-127:
the driver charges CompletedRows while draining and CompletedBytes/
CompletedAbiBytes once from the frozen result. A completed-storage builder
charges its real copied/value work and capacity, without duplicating those
driver counters. Actual ZE-127 composition remains a later integration gate.

## RED, correction and independent controls

The initial named tracer/kernel tests failed to compile against absent APIs
(logs 01, 04, 08, 10, 12, 14 and 20). Those compiler failures establish missing
seams, not behavioral correctness. Subsequent concrete REDs were:

| Evidence | Observed failure and correction |
| --- | --- |
| 16-empty-cancel-red | Empty sort returned success after cancellation; check entry even when there is no loop work. |
| 23-eligible-max-red | Sorting an already ascending maximum set exhausted value-work budget; preserve the already known order. |
| 25-sort-counter-red | Sorting 70,000 rows reported zero examined row work; charge actual input scanning. |
| 29-high-hash-red | Same-low/different-high IDs exhausted hash probes; mix every ID byte before selecting buckets. |
| 42-retained-eager-red | Fresh anonymous preparation lifetimes could not populate retained RowBatch; bind prepare_search to the same execution lifetimes as pull. |
| 48-signed-oracle-red | Independent oracle sorted u128 transport encodings of signed inputs; sort original i64 values before encoding. Literal [-3, 1, -3] expects encoded [-3, -3, 1]. |

Runtime REDs exited 100; the retained-eager compiler RED exited 101. Final
tests below are GREEN. An early cancellation-test attempt had an unrelated
fixture memory allowance failure; it is not the cancellation evidence. Log 16
is the subsequent intended assertion failure with the corrected fixture.

The parent independently reviewed the frozen production sources and identified
the signed-number oracle defect. The correction also changes the seeded number
domain from 0..30 to -15..15; generated NodeIds remain positive 1..31 plus high
identity bits. Same-seed deadline controls still fire and pass.

[mutations.json](ze-125/mutations.json) and logs 32–36 record actual deliberate
controls. Forcing every hash to zero leaves both DISTINCT and grouping GREEN,
proving collision handling. Removing collision equality, breaking stable tie
ordering, bypassing same-view validation, and suppressing examined-entry
accounting each produce runtime RED (100). All mutations were restored and the
source hashes verified before terminal tests. These mutations were performed
only in this owned worktree, with no moving mutant source given to reviewers.

## Terminal verification

Host: Apple M3 Max, 16 CPUs, 128 GiB RAM, macOS 27.0 arm64; rustc 1.93.0
`254b59607`; cargo-nextest 0.9.145 `00af4550e`. Tests use the repository's nextest
default profile: four isolated test processes, retries zero, one libtest thread
per process. This is macOS component evidence, not other-platform qualification.

```sh
cargo nextest run -p zeppelin-embed --features allocation-audit \
  --test graph_relational --test graph_query_runtime \
  --test graph_query_runtime_control --success-output final
# 32/32 passed: 13 relational plus 19 runtime. Log 44.

cargo nextest run -p zeppelin-embed-adversarial-oracle \
  -E 'test(relational_oracle)'
# 2/2 passed, 109 skipped. Log 49.

cargo nextest run -p zeppelin-embed-workspace-tests --test adversarial_tests \
  -E 'test(property_graph_runtime) | test(property_graph_relational) | test(relational_contracts)' \
  --success-output final
# 3/3 passed, 455 skipped. Log 50.

cargo test -p zeppelin-embed --doc OperatorFactory -- --show-output
# 1/1 intended compile-fail doctest passed. Log 46.

cargo clippy -p zeppelin-embed --features allocation-audit --lib \
  --test graph_relational --test graph_query_runtime_control --message-format=json
# Exit 0; zero compiler-message diagnostics. Log 47 and lint-summary.json.
```

Total focused terminal result: **37 nextest tests and one lifetime rejection**.
Logs 38/45 retain earlier oracle/runner GREENs; logs 49/50 supersede those after
the signed oracle correction. Core/lifetime/lint source is unchanged by that
two-file test correction. Final formatting wraps two existing test/coverage
expressions and removes one trailing blank line; it changes no executable code.

The actual retained-buffer tracer composes three stages across seven batches.
The production-chain test composes RowSource, Blocking DISTINCT and MapRows.
The eager test fills a real retained RowBatch from two sources in source order,
then verifies it under LIMIT 0; second-source failure prevents pull and releases
all charges. Existing close-first, completion, control and cumulative-limit
tests remain GREEN. The pre-change PG9 runner probe (log 02) and the final
PG9 probe both pass.

Actual allocation failure sweeps cover all **32 relational allocation positions**
and **two eligibility positions**, with zero unattributed allocation bytes,
observed failure at every selected position, a clean control, and released query
and shared reservations. At **524,288 IDs**, the set's actual retained capacity
is 524,288, live query reservation delta **8,388,720 bytes**, and transient query
peak **16,777,904 bytes**. Existing limits are unchanged. These are deterministic
capacity/counter measurements, not latency or throughput claims.

PG14 adds eight append-only `property-graph.relational.*` coverage keys. For each
seed 0, 1, 125 and u64::MAX, the real sort/aggregate/eligibility kernels run six
cases: three scheduled-clock faults and three matching clean controls. Each
selected fault fires exactly once, returns typed Timeout without a successful
prefix, and releases ownership. The actual seeded runner executes **59
operations, zero violations**, reaching all eight keys. The primitive oracle
uses standard integer sorting/BTreeSet independently of production value/hash
helpers and rejects wrong bags/order, truncated identities and false fault/leak
evidence. Typed synthetic inputs qualify these kernels only.

## Remaining acceptance

ZE-50/51 must compose the real graph/evaluator producers, structured plan
operators and complete native execution. ZE-64 retains actual eager ranking
source/report acceptance. ZE-53/127 retain completed-storage/public lifetime
integration; ZE-56 retains real public read lowering and TCK execution. No
lower-plan or column-copy check substitutes for those gates.

Nonessential broad workspace/adversarial campaigns and whole-crate coverage
remain in backlog ZE-118, including this candidate's exact source paths and
eventual integrated revision. The recorded deferred commands are
`cargo nextest run --workspace`,
`cargo nextest run -p zeppelin-embed-workspace-tests --test adversarial_tests`,
and `scripts/coverage.sh`, with ZE-118's final instrumented nextest matrix as the
combined execution plan. No full-suite GREEN or >=90% per-crate result is
claimed. The earlier no-feature strict Clippy invocation reported inherited
warnings; the scoped allocation-audit invocation above has zero diagnostics,
and is the lint result claimed here.
