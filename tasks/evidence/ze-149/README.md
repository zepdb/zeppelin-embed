# ZE-149 typed native error transport

Source pin: `ce7c5f948e60287697b395e1b27d6363b9f79b6b`.
Accepted plan: `astra-plan.md`, SHA-256
`3b736624c60051e2e75702b456b5becea4907ab508e442b9a43510130f17244a`.

## Result

The existing single pull driver, factory, completion seam, `MapRows`, and
`BlockingRows` now carry an inferred error type whose default remains
`RuntimeError`. The crate-private `NativeExecutionError` retains exact runtime,
expression (including `ExprId`), plan, and tree payloads without boxing or
string conversion. `RuntimeError`, native producers, kernels, evaluator,
storage, and `native_graph.rs` are unchanged.

This transport-only change adds no production native operation or fault site.
The injected failures prove transport only; future real native consumers must
register and qualify their own changed work and failure seams.

## RED to GREEN

Behavioral RED command:

```text
cargo nextest run --profile default -j 4 --retries 0 -p zeppelin-embed --features graph-cypher --lib -E 'test(native_error_transport_late_pull_preserves_expression_and_tree)'
```

Nextest run `427aea86-323b-42f7-8008-eba2dacf8335` executed the real
`execute_in` drain after one private row and failed at the intended assertion:
observed `Lost`, expected `ExpressionTree { expression: ExprId(37), detail:
"ze-149 retained native tree detail" }`. The failure was behavioral, not a
missing-method or unavailable-type compiler error.

After the additive transport change, the exact four-test selection passed:

```text
cargo nextest run --profile default -j 4 --retries 0 -p zeppelin-embed --features graph-cypher --lib -E 'test(native_error_transport_)'
```

Nextest run `e68f48c9-3f8c-4329-aaa7-f03f46d5549f`: 4 passed, 628 skipped,
zero retries. The cases cover late pull, relational wrappers, factory/eager/
completion failures, default inference, close-first precedence, exact counters,
and reservation release.

## Accepted fixture addendum

The original five runtime-control regressions failed before query execution at
`Store::open`: `BudgetExceeded { needed: 573752, budget: 262144, component:
"temporary" }`. Root independently reproduced this on unchanged source pin
`ce7c5f9` in nextest run `e4d6f0e7-0b14-4292-a0a3-e5aad0cf426b`; raw evidence
is `/tmp/ze-149-main-runtime-control-baseline.log`.

The Astra planner and root approved `/tmp/ze-149-fixture-addendum.md`: only the
two `graph_query_runtime_control.rs` Store fixture caps changed from 262144 to
`1024 * 1024`. Both 131072-byte `QueryMemory` caps, every runtime/result/work
limit, and every assertion remain unchanged.

With that fixture-only correction, the original selections passed:

- `graph_query_runtime_control`: nextest run
  `c352cd31-ef9d-44b8-bfaf-172f0650ac2c`, 5 passed, 2 skipped, zero retries.
- `graph_relational`: nextest run
  `73a79a5f-5c88-4dad-a59a-0c6048acfac6`, 2 passed, 9 skipped, zero retries.

## Compile controls

Each required command passed with `-j 4`:

```text
cargo check -j 4 -p zeppelin-embed --lib
cargo check -j 4 -p zeppelin-embed --features graph-cypher --lib
cargo check -j 4 -p zeppelin-embed-cypher --test runtime_lowering
cargo check -j 4 -p zeppelin-embed-ffi --lib
cargo check -j 4 -p zeppelin-embed-ffi --features graph-cypher --tests
cargo check -j 4 -p zeppelin-embed-ffi --features graph-result-test-support --tests
cargo check -j 4 -p zeppelin-embed-workspace-tests --test adversarial_tests
cargo check -j 4 -p zeppelin-embed-workspace-tests --features graph-cypher --test adversarial_tests
cargo check -j 4 -p zeppelin-embed-workspace-tests --features graph-result-test-support --test adversarial_tests
```

No runner execution, full/workspace suite, coverage, soak, fuzz, benchmark, or
platform qualification was run. Those broader obligations remain with ZE-118.
