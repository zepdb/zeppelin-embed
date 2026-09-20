# ZE-154 native relational execution evidence

Date: 2026-09-20

Branch: `codex/ze-154-native-relational`

Base: `f08898e6d34c3983ab07dc085567481e2ae81c18`

Frozen implementation plan SHA-256:
`9bcc03d2bc909724ae8d76fec5f318d703313fd337f73181f047c6035f472063`.
The exact plan is retained as `astra-plan.md`.

Focused-scope review SHA-256:
`c960609e30b87a03ab437cdb4fbd59e78c068ba9a62de41e0adf05f8784452ce`.

Stack-regression correction SHA-256:
`2c99d5ea690ca52a240d7dbcc4e16325151b1815c8ac6a1cb0c5f23bfc7b1390`.

Stack-localization SHA-256:
`ce144eeb74b58654ac41de0831268082fbb98b9da5f42c3d4a6259c0fd29e79d`.
Its static evidence SHA-256 is
`7991af201694ceff19640e91741529a71fd38553069d789f97698ce0d33aa9a0`.

## Implemented behavior

Native pattern execution now handles `OffsetLimit`, `Sort`, `Distinct`, and
`Aggregate` occurrences through the existing relational kernels and runtime.
The blocking operators preserve the visible sparse row separately from hidden
sort/group values, retain relationship-use sidecars, restore state across
errors and resets, and charge owned state through the query memory account.
Aggregate lineage passes only an exact same-slot group-key expression; aggregate
outputs do not inherit anchor lineage.

Native eligibility construction now evaluates a singleton input domain exactly
once, exhausts it before construction, preserves the charged row, and builds a
complete checked `EligibleNodeSet`. Omitted, empty, null, duplicate, cap,
foreign-owner, and typed-member behavior is covered by the focused native test.
The foreign-owner control uses a second admission to the same store and
same generation while the first owner remains retained.

Sort, distinct, and aggregate states use one-element charged `QueryArena`
owners built outside recursive occurrence construction. Checked arena access is
inside the existing operator helpers and reset/lineage paths. This preserves
error restoration while avoiding the recursive dispatch-frame growth observed
in the first frozen native selection.

The approved test-only close observer waits on the actual retained native read
lease cancellation publication. Its two-file patch exactly matches
`/tmp/ze-154-156-cancellation-observer.patch`, SHA-256
`4663157cd90bfbecd0c7f2045b70d8facb1863d2d1a88117d1900d2549c97bd6`.
It does not alter production lifecycle state or close semantics.

## Literal RED and GREEN

First behavioral RED:

- Command: `cargo nextest run -p zeppelin-embed --lib --features graph-cypher -j 4 --retries 0 -E 'test(native_relational_offset_scope_then_match)'`
- Run ID: `5b56ae4e-bf49-4e83-90c9-9be3da33e2bf`
- Result: 0 passed, 1 failed with `Some(Build(Plan(Reference)))`.

First useful GREEN after the smallest native `OffsetLimit` occurrence change:

- Same command and fixture.
- Run ID: `0138f410-411b-46e8-8117-a8848ecf056b`
- Result: 1 passed, 0 failed.
- Raw transcript: `offset-first-red-green.log`.

Stack-regression RED after the initial blocking-state integration:

- Frozen native selection run ID:
  `c20cf771-2f58-4c3c-b057-e69390f39856`.
- Result: 17 passed, 1 SIGABRT from stack overflow in
  `native_pattern_keys_labels_liveness_full_ids`.
- Raw transcript: `final-native-selection.log`.

Stack-regression GREEN after the approved source-local correction:

- Isolated unchanged test run ID:
  `06b6ee43-79a8-4154-befc-e286515b98d9`.
- Result: 1 passed, 0 failed.
- Raw transcript: `stack-regression-green.log`.
- Final layouts were `PhysicalState` 1392 bytes and `Occurrence` 2000 bytes;
  the exact compiler layout output is in `stack-layout-after.log`.

## Final verification

The frozen native selection passed 18 of 18 tests with run ID
`a4bc8043-5b2b-4269-a4e8-648405d91ab2`. This selection includes the eight
focused ZE-154 groups and ten unchanged native-pattern regressions. Raw output
is in `final-native-selection-green.log`.

The existing `graph_relational` integration test binary passed 11 of 11 tests
with run ID `f26ae397-d065-448a-89b1-37f18fa25f38`. Raw output is in
`graph-relational-green.log`.

The following narrow build matrix passed. Full output is in
`final-checks.log`.

- `cargo check -p zeppelin-embed --lib -j 4`
- `cargo check -p zeppelin-embed --lib -j 4 --features graph-cypher`
- `cargo check -p zeppelin-embed --lib -j 4 --features graph-cypher,test-support`
- `cargo check -p zeppelin-embed --lib -j 4 --features graph-cypher,allocation-audit,query-timing`
- `cargo check -p zeppelin-embed-cypher --lib -j 4`
- `cargo check -p zeppelin-embed-ffi --lib -j 4`
- `cargo check -p zeppelin-embed-ffi --lib -j 4 --features graph-cypher`
- `cargo clippy -p zeppelin-embed --lib -j 4 --features graph-cypher`

The build matrix completed with existing warnings and no errors. No broad,
full-workspace, adversarial, performance, or platform qualification was run;
those were outside the frozen ZE-154 execution scope.

## Main integration

Root cherry-picked the reviewed implementation onto main, preserving the
already-integrated identical cancellation observer, then registered the actual
native-relational probe and its ten receipt keys in the shared runner.

The frozen integrated selection passed 18/18 in nextest run
`7d0b5aac-dcee-45f8-8566-6fe80ce4f154` with 667 tests skipped. The registered
adversarial consumer compiled successfully under default, graph-cypher, and
graph-result-test-support features. Raw output is in `main-focused.log` and
the three `main-consumer-*.log` files. These are focused execution and actual
consumer compile checks; the broad adversarial campaign remains deferred.
