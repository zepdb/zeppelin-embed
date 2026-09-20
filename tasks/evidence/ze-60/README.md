# ZE-60 view-bound retrieval adapter evidence

ZE-60 is based on `439237bd382e6186cf6ba869acf0d817284b8937` on branch
`codex/ze-60-view-search-adapters`. The approved Astra plan is preserved as
[`astra-execution-plan.md`](astra-execution-plan.md), SHA-256
`ae61659beebcd194249a6bbdfe2c9df2169ca99aadaf64d281fd96fa7a8815dd`.

The checks ran on an Apple M3 Max MacBook Pro (`Mac15,9`, arm64, 128 GB), macOS
27.0 build `26A5388g`, rustc 1.93.0, and cargo-nextest 0.9.145.

## Result

The new private adapter retains the admitted `GraphReadView`, exact query-view,
runtime and query-memory identities, and one charged control reservation. It
preserves full-width `NodeId` and revision identity, authentic staged node
versions, borrowed vector input and `SearchMode`, same-view eligibility, real
native lookup/version validation, and source-bound payload access. Text copies
use bounded native spans. Text and vector output copies are charged as actual
copy work, and absent payload remains distinct from present empty text.

The shared vector leaf preserves the legacy empty/maximum/nonfinite error order
and accepts a typed fallible control callback. The legacy caller retains its
65,536-coordinate quantized limit; an actual native declaration and Exact
request at 65,537 coordinates succeeds. The root-approved fixture-only change
parameterizes the existing actual producer dimensions while leaving every old
fixture call at dimension 2; no broader fixture refactor was made.

The shared version leaf is allocation-free. Existing materialization retains
the original `MaterializationError::IdentityMismatch` row and formatting, while
native resolution rejects missing, tombstoned, and stale versions.

## RED and GREEN

Three direct product mutants supplied substantive RED controls and were
restored byte-for-byte before terminal GREEN:

- Narrowing native node conversion to the low 64 bits made
  `ze60_full_width_identity_and_staged_node_versions` fail with both distinct
  values observed as `DocId(4276993775)` (run
  `9972b4a3-8deb-43f5-ae08-eaa4840bd8dd`). Restored GREEN:
  `f620f4c7-c56a-4c5e-bfbe-d0805444ab18`.
- Bypassing `EligibleNodeSet::ids_for` exact-view validation made
  `ze60_rejects_foreign_eligibility_and_runtime_before_reads` accept the
  foreign set and fail its assertion (run
  `2e26b70e-3ffe-46c8-bd8e-3b5f555ab2ec`). Restored GREEN:
  `992f6c85-5290-423c-9ec3-fafe2100af89`.
- Bypassing `require_document_version` made
  `ze60_resolves_version_and_payload_from_retained_native_view` accept the
  wrong revision and fail its assertion (run
  `a353652c-fa93-40a5-b7e4-eea5ea34c368`). Restored GREEN:
  `829c5188-7552-4920-8c02-9d9770d5496b`.

The preparation test was GREEN in run
`68676b8a-e209-4a32-95c2-cefc7b60c25b`. The cleanup test initially exposed a
test barrier race: store state can become Closing before native lease
cancellation is observable. The final proof reuses the existing
`publication.state`/`publication.changed` barrier and waits for a real witness
lease to report `ReadCancelled`; it does not poll, sleep, or weaken the
close-first assertion. Its terminal GREEN is
`14ba7407-9404-4c80-b3f7-cf76f4b941f9`.

The terminal named command was:

```text
cargo nextest run --profile default -j 4 --retries 0 -p zeppelin-embed --features graph-cypher --lib -E 'test(ze60_)'
```

Run `add1747d-b490-44d9-87b4-754cc6a55808`: 5 passed, 605 skipped.

The tests assert a cumulative limit refusal after exactly 1 vector coordinate
and 4 vector bytes; foreign eligibility/runtime failures leave lookup and vector
counters unchanged; two source-to-field plus two field-to-output f32 copies add
exactly 16 copied bytes. Context construction refusal and successful drop return
query memory to the measured baseline. Copy-out returns exact original f32 bits
after admission ends. Store active-query, mapped-byte, and resident-owned-byte
statistics return to baseline, and close remains blocked until live leases and
the callback drain.

## Focused regressions and feature controls

```text
cargo nextest run --profile default -j 4 --retries 0 -p zeppelin-embed --features graph-cypher --lib -E 'test(exact_and_graph_tiers_preserve_quantizer_validation_errors) | test(native_read_scoped_consumer_retains_one_catalog_and_bundle) | test(native_read_cursor_rejects_same_view_memory_different_runtime) | test(native_read_graph_only_optional_payloads_and_full_width_ids)'
```

Run `22c1c016-b2fd-4987-8d52-a837b446ba59`: 4 passed, 606 skipped.

```text
cargo nextest run --profile default -j 4 --retries 0 -p zeppelin-embed --test store_text_columns -E 'test(astra_07_result_text_and_revision_match_scored_snapshot) | test(astra_07_missing_text_or_identity_is_typed_and_store_remains_reusable)'
```

Run `04a733d5-881a-4058-9b55-8a1b2f6c7fea`: 2 passed, 29 skipped. These are
the existing focused legacy regressions that exercise the extracted version
equality leaf.

All exact compile controls exited 0:

```text
cargo check -p zeppelin-embed --lib --no-default-features
cargo check -p zeppelin-embed --lib --features graph-cypher
cargo check -p zeppelin-embed-workspace-tests --test adversarial_tests --no-default-features
cargo check -p zeppelin-embed-workspace-tests --test adversarial_tests --features graph-cypher
cargo check -p zeppelin-embed-workspace-tests --test adversarial_tests --features graph-result-test-support
cargo fmt --check -p zeppelin-embed
cargo clippy -p zeppelin-embed --lib --features graph-cypher -- -A warnings -D clippy::indexing_slicing -D clippy::panic
git diff --check
```

The adversarial target was compiled only; no adversarial test was executed.

## Lifetime controls

Each probe used an isolated current-source copy with the corresponding evidence
file included as a private crate module, then ran:

```text
cargo check -p zeppelin-embed --lib --features graph-cypher
```

[`lifetime/negative.rs`](lifetime/negative.rs) exits 101 because returning the
resolved native node as `'static` requires the admitted source lifetime `'s` to
outlive `'static`; exact output is in
[`lifetime/negative-current.log`](lifetime/negative-current.log).
[`lifetime/positive.rs`](lifetime/positive.rs) copies text bytes and vector bits
inside admission into lifetime-free owned values and exits 0; output is in
[`lifetime/positive-current.log`](lifetime/positive-current.log).

## Runner impact and qualification boundary

Inspection found no changed adversarial-runner source and no new storage fault
site: the adapter uses existing runtime checkpoints, counters, native lookup,
bounded payload reads, memory reservations, and lease cancellation. ZE-118 must
still cover the integrated changed paths: the new retrieval and view-binding
modules; shared vector validation and both callers; shared version validation
and both callers; exact-view eligibility refusal; full-width identity; native
dimension, work, cancellation, deadline and memory refusal; missing/stale/
tombstoned resolution; bounded text/vector copying; and reservation/lease
release. Its matrix must include no graph feature, `graph-cypher`, and
`graph-result-test-support`, plus the deferred full adversarial execution,
per-crate coverage, fuzz, size, soak, performance, platform, and release gates.

This ticket does not implement or qualify sparse index population, statistics,
ranking, anchors, ANN/fusion, a search operator or report, durable publication,
checkpoint/recovery/GC, compiler/ABI/Swift surfaces, or platform/release work.
