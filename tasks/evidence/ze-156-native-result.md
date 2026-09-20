# ZE-156 native result materialization evidence

## Scope and environment

This evidence covers the private native read consumer that materializes rows
from `NativePattern::execute_in` into ZE-127 `PreparedGraphResult` storage and
detaches only after a successful final `Execution`. It does not qualify public
native-to-C composition, broad adversarial execution, compaction, search, or
the original ZE-53/52/64/66/68/69 requirements.

- Branch: `codex/ze-156-native-results`
- Base: `e13c48c6d7cbf1dafb0704888c191d6582830046`
- Host: Apple arm64, macOS 27.0 build 26A5388g
- Toolchain: `rustc 1.93.0`, `cargo 1.93.0`
- Frozen execution plan SHA-256:
  `dcfe495cf1a03b977924d0f0dea3930d798f715f6d39ac952d643e3a33c89b0c`
- Frozen readiness report SHA-256:
  `2b0082808d64b4f20827ce8d70752f29ab1b9f3ab25d4f4b8955408db5405233`

The shared test-only cancellation observer was applied byte-for-byte from
`/tmp/ze-154-156-cancellation-observer.patch`, SHA-256
`4663157cd90bfbecd0c7f2045b70d8facb1863d2d1a88117d1900d2549c97bd6`.
The seeded fresh-store fixture is documented in
`tasks/evidence/ze-156/high-id-fixture-addendum.md`, SHA-256
`072154dd435f7c19b424dec6a57fdd854e2e3547d44bfd826b6f3db3ddf6a51c`.
Its formatted persistence-only patch is frozen at
`/tmp/ze-156-high-id-fixture.patch`, SHA-256
`5640c4cfc6f92c78e1ec0f092bec3b106efd3dbbd90829412bd6e3847b3e405c`.

## RED to GREEN

The first behavioral test was run alone:

```text
cargo nextest run -p zeppelin-embed --lib --features graph-cypher \
  -j 4 --retries 0 \
  -E 'test(native_result_actual_rows_survive_close)'
```

RED reached the intended unimplemented completion boundary with
`RuntimeFailure { operator: PlanNodeId(2), error: Completed(Shape) }`.
After the native source-to-owned translation was wired, the same named test
passed: 1 passed and 666 skipped.

The genuine high-ID fixture first failed with the allocator returning
`NodeId(1)` where literal `H - 1`, for `H = 1 << 80`, was required. The
seeded fresh constructor then passed ordinary empty reopen, native writes,
materialization, populated reopen, and next-ID monotonicity. The terminal
group run was `ec9fae56-4dc0-4d9e-bc50-08cd4bb7cd53`: 1 passed and 673
skipped. The admitted empty bundle asserted node high water `H - 2`,
relationship high water `(1 << 96) + 6`, generation zero, sequence zero,
and empty roots. The populated ordinary reopen asserted the written high
waters, changed generation, sequence one, and nonempty roots.

Two attempted fixture budgets, 32 MiB and 64 MiB, were invalid because the
existing hard maximum is 24 MiB. Both failed constructor validation before
consumer execution; they were not allocation-pressure failures. The fixture
uses the existing 24 MiB maximum and no budget was widened.

The initial close schedule observed `StoreState::Closing` during the normal
reader grace period and allowed completion. That result was retained as a
diagnosis, not treated as a product bug. The final test sets the reader drain
timeout to zero and uses the shared observer to wait for actual lease
cancellation before the unchanged runtime checkpoint. It then returns typed
`ReadCancelled` with positive copied work and no completed owner.

The first VFS schedule armed after materialization and only saw cached maps,
so it was discarded as evidence. The final directed case uses three separately
committed node artifacts and arms a thin source wrapper after the first real
nonempty pull. The second real source read fails the named `open_for_map` path,
preserves the typed tree I/O error, records prior row work, returns no completed
result, and releases resources. The terminal controls group run was
`ea607460-8ef9-4af5-b8ca-5756b7df133b`.

## Focused acceptance

The exact eight named groups were run with:

```text
cargo nextest run -p zeppelin-embed --lib --features graph-cypher \
  -j 4 --retries 0 -E 'test(native_result_)'
```

Nextest run `5327c6c4-0161-420b-acd0-78fc821e2b2f` passed all eight groups;
666 tests were skipped. The committed source covers actual rows surviving
close, checked records and properties, stored and query list/scalar fidelity,
full-width IDs, same-view and owner rejection, measured copy/completed/memory
limits, cancellation/deadline/arithmetic/VFS failures, exact final counters,
release loops, and the eight directed receipts. The receipt names are `copy`,
`identity`, `same-view`, `limit.fire`, `control.fire`, `late-error.fire`,
`release`, and `oracle.can-fire`.

The unchanged ZE-141 owned-storage consumer was run once:

```text
cargo nextest run -p zeppelin-embed \
  --test graph_completed_results --features graph-cypher \
  -j 4 --retries 0
```

Nextest run `5ceba287-4aae-4cca-8eda-ba16008df353` passed all 15 tests.

## Finite compile checks

These commands passed:

```text
cargo check -p zeppelin-embed --lib -j 4
cargo check -p zeppelin-embed --lib -j 4 --features graph-cypher
cargo check -p zeppelin-embed --lib -j 4 --features graph-cypher,test-support
cargo check -p zeppelin-embed --lib -j 4 \
  --features graph-cypher,allocation-audit,query-timing
cargo check -p zeppelin-embed-cypher --lib -j 4
cargo check -p zeppelin-embed-ffi --lib -j 4
cargo check -p zeppelin-embed-ffi --lib -j 4 --features graph-cypher
cargo check -p zeppelin-embed-ffi --lib -j 4 \
  --features graph-result-test-support
cargo clippy -p zeppelin-embed --lib -j 4 --features graph-cypher
```

Clippy completed with the repository's existing warning set and no warning in
the new native completion files. No broad, advanced, full, adversarial,
coverage, fuzz, sanitizer, soak, performance, size, or release suite was run.
Root owns registration of the eight probe keys and compilation of the
registered actual consumers. The high-ID evidence does not claim the separate
same-low-64-bit collision matrix retained by ZE-53/62.

## Preservation

The inherited file hashes remained:

```text
.gitignore       8e19c948be4fa3ac026e7cb833118744fb50a4b3b4c87844a0c156497166255b
AGENTS.md        e34cf1436cd36f815a941dc64113932cabd4096c78be0ed0493ff09c812d8e97
README.md        0126721b94a3b3fd8f23c7a99d89a07b83d61332055e0c19e025635d078449cc
CONTEXT.md       df909e56a30ee1d9c64e047077a9c9260237b78275058807f3f4ab6559be8cb1
plan.md          1ee843ba44604a55118c5f1c24b0c59157bc15e4d44a524320543e4513346822
skills-lock.json a0811ce6d4364c99a5261e8ad683ed6795203607c30ee359a9d4c332b7759ec1
```

The inherited symlinks remained `.agents`, `tracker`, and `CLAUDE.md`.
