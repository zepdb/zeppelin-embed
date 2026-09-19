# ZE-124 independent frozen-source review

Verdict: one required verification blocker. No additional correctness blocker found in the bounded production-code review.

Reviewed `/tmp/ze-124-review-1`; manifest SHA-256 `ab8662631d78cfdc2afe50c73c710a1582db98bdde781732632d886b4a8e6887` and all 11 listed file hashes match. Compared against the live ZE-124 ticket, frozen approved design, `parallel-contracts.md` adjacency seam, and relevant storage-plan sections. Shared outer-tag edits were diffed against base `7028a42f2f4898bb7aa729dc935bd51b4033f2ba`. Neither moving worktree nor tracker was edited.

## Finding

**P1 — the required allocation-audit unit test does not compile.** Frozen `crates/zeppelin-embed/src/property_graph/storage/adjacency/tests.rs:56` contains:

```rust
&mut |_| Ok::<_, u8>()
```

`Ok` needs its unit argument: `Ok::<_, u8>(())`. The module is gated by `cfg(all(test, feature = "allocation-audit"))` in `adjacency/mod.rs`, so ordinary `graph_adjacency` integration tests do not expose this error. This blocks the new actual-allocation proof for successful encode/merge and typed format/limit/control errors.

A minimal compiler reproduction using the exact closure expression was run via rustc stdin (no source edits):

```sh
rustc --edition=2024 --crate-type lib -o /tmp/ze-124-review-constructor.rlib - <<'RS'
fn frozen_control() -> impl FnMut(()) -> Result<(), u8> {
    |_| Ok::<_, u8>()
}
RS
```

Observed exit 1, `E0061: this enum variant takes 1 argument but 0 arguments were supplied`; compiler suggests adding `()`. This is a narrowed syntax reproduction, not a full candidate test run. Correct the test and run only the required focused allocator check, for example:

```sh
cargo nextest run -p zeppelin-embed --lib --features allocation-audit \
  -E 'test(adjacency_actual_allocator_reports_zero_for_encode_merge_and_all_error_classes)'
```

The root and ZE-124 owner were notified before this report was written.

## Production audit

- The 96-byte hand-written header and 32/40-byte entries match the frozen design. Decode rejects unsupported tags, nonzero reserved bytes, noncanonical infinity, zero identities, exact-length/count disagreement (including trailing bytes), wrong group identity, non-strict numeric ordering and out-of-range entries. Strong ID constructors were checked at the pinned base: they return compact errors without allocating. Both binary goldens were independently decoded with Python: base sequence4 entries `(255,2),(2^64,3)`; delta sequence5 deletes `(255,2)` and inserts `(u128::MAX,4)`, with exact action/reserved bytes and geometry.
- Merge binds the base watermark exactly, checks watermark <= cutoff, requires watermark < every delta sequence <= cutoff and nondecreasing run sequences, and enforces eight runs/2048 pending observations. The bounded nine-cursor walk visits inputs in that validated sequence order. Every repeated RelId must retain its neighbor across all supplied sequences; equal-sequence actions must match; exact duplicates coalesce. A later delete cannot hide earlier topology corruption.
- Full validation and the first complete cross-run walk finish before any output copy. That walk determines surviving count and split identity, so output-capacity failure precedes mutation. A second walk copies only survivors, then the final checkpoint gates the sole successful output borrow. Control failure can leave private scratch modified, as documented, but returns no `Merged` result or row callback.
- Comparisons use the derived full-u128 ID order. The split records the actual 4097th surviving RelId, retains original outer endpoints, and returns adjacent nonempty intervals with 4096 entries in the first. At most6144 survivors are possible, so two partitions suffice; empty output has none. No successor arithmetic is performed on maximum IDs. Checked or preceding fixed bounds protect length/offset/count arithmetic.
- Header/entry/copy work is bounded by96 bytes per callback, and actual entries and merge comparisons checkpoint. Fixed run/cursor arrays and borrowed caller buffers introduce no heap owner, formatting or iterator allocation. Inner errors retain the caller control error. `MERGE_STATE_BYTES` explicitly describes only simultaneously live run/cursor arrays; the integration owner must additionally account its ordinary stack and retained backing as already specified.
- Shared edits append only tags13/14 to `BlockKind` and artifact/WAL decoding; existing tags remain unchanged and no PayloadRef role is widened. ZE-43's reserved11/12 must still be composed by the integration owner; no unfinished ZE-43 code appears in this snapshot.

## Evidence and remaining scope

Read the focused tests covering byte goldens, all truncations, reserved/action bytes, late topology corruption, full IDs/infinity, empty/finite ranges, 4097/6144 splitting and reencoding, equal-sequence conflicts/duplicates, ninth-run/2049-entry rejection, output limits, and refusal at every observed merge/base-encode checkpoint including final completion. Those source checks were inspected, not rerun or represented as independent pass results.

The snapshot does not include the eventual seeded-runner integration/independent edge-oracle evidence; this review neither claims those final ZE-124 gates passed nor replaces them with source inspection. Actual ZE-44 OUT/IN participants, authoritative topology, endpoint liveness, admitted sequence/cutoff binding, real resource accounting, and ZE-45/47 lease/publication/recovery acceptance remain their existing owning tickets. No broad suite is requested here.
