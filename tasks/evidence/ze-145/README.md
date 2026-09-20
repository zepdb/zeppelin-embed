# ZE-145: native graph scalar expression evaluation

Implemented the crate-private scalar evaluator over a retained `RuntimePlan`,
explicit `Schema`/`RowBatch` row, authentic `RuntimeContext`, and admitted
`GraphReadView`. The evaluator owns bounded reusable scalar/list/string scratch,
copies and recursively proves retained parameters, dispatches every nonaggregate
expression form through the existing value kernels, rejects aggregate use, and
keeps runtime, plan, and native tree failures typed with the requested `ExprId`.

Native reads resolve catalog symbols and consume actual node/relationship
properties, labels, relationship types, stored text, and full-width identities.
Canonical property tags 1 through 9 are decoded through bounded payload cursors;
native and caller strings/lists are copied into charged scratch. List reservation
and metadata scans poll the existing runtime/tree control for every cell.

## RED -> GREEN

- RED: `native_expression_scalar_dispatch_and_parameters` compiled the complete
  admitted ownership chain, then the deliberate arithmetic tracer returned
  `I64(41)` where the literal expected value was `I64(42)`.
- GREEN: the evaluator dispatched the expression through checked
  `QueryValue::arithmetic`; the focused test passed and charged exactly three
  expression visits for that initial tree.

## Focused acceptance

The four `native_expression_` tests use an actual prepared native generation.
They cover scalar dispatch and recursively backed parameters; every canonical
property/list kind; labels, relationship types, absent/empty/zero-term and
multi-chunk text; shared-low-bit full identities; hidden relationship endpoints;
same-view/runtime/memory ownership; controlled generation replacement; copied
output reuse; exact cumulative expression limits; preallocation refusal and
charge release; undersized scratch; late-error reset/destination atomicity;
deterministic cancellation inside list/text polling; and evaluator close-first
precedence over caller cancellation and malformed input.

Final focused results on 2026-09-20:

```text
cargo nextest run --profile default -j 4 --retries 0 -p zeppelin-embed --features graph-cypher --lib -E 'test(native_expression_)'
4 passed, 605 skipped

cargo nextest run --profile default -j 4 --retries 0 -p zeppelin-embed --features graph-cypher --lib -E 'test(native_read_scoped_consumer_retains_one_catalog_and_bundle) | test(native_read_cursor_rejects_same_view_memory_different_runtime)'
2 passed, 607 skipped

cargo check -p zeppelin-embed --lib
passed

cargo check -p zeppelin-embed --features graph-cypher --lib
passed

git diff --check
passed
```

This is component evidence only. Pattern and relational consumers, mutation
overlay integration, public completed results, compaction/reopen, TCK, ABI, and
broad/adversarial qualification remain with their existing tickets.
