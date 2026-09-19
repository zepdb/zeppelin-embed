# ZE-124 bounded delta/final review

Verdict: previous verification blocker resolved; no new blocker found in the
reviewed delta. This is component review, not ZE-44/45/47 integration acceptance.

Frozen source: `/tmp/ze-124-review-2`, manifest SHA-256
`5f686fdeb2d775cb314f6a025f960fc23bb6207d4dcafef69d2ec79ff033cc5d`.
Independently verified all 22 listed hashes and corresponding tested ZE-124
worktree bytes. Prior review: `/tmp/ze-124-independent-review.md` and snapshot 1.

Production adjacency `mod.rs`, `codec.rs`, and `merge.rs` are byte-identical to
the already reviewed snapshot. Artifact/storage production edits and both
binary goldens are also unchanged. The WAL codec difference removes only its
private test; the replacement integration test exercises append-only tags
13/14, existing tag 10, and refusal of 15 through actual public ReferenceList
decode. The additional high-ID merge test spans separate base/delta runs.

## Resolved finding

The allocation-audit closure now correctly supplies `Ok::<_, u8>(())`.
Independently ran the actual feature-gated test, not only a syntax reproduction:

```sh
cargo nextest run -p zeppelin-embed --lib --features allocation-audit \
  -E 'test(adjacency_actual_allocator_reports_zero_for_encode_merge_and_all_error_classes)' \
  --test-threads 4 --retries 0 --success-output final
```

Exit 0; nextest run `55647b6f-72ea-4441-8815-cf853cdc89f5`, one test passed.
Actual allocator audit reports zero calls on encode/merge success, malformed
reserved bytes, output-capacity rejection and final-checkpoint cancellation.
Caller buffer bytes 296; declared cursor/run representation 1224. Raw independent
output: `/tmp/ze-124-independent-allocation.log`. The production/source hashes
remain the reviewed snapshot hashes.

## New delta

- The std-only PG12 oracle uses primitive u128 pairs and map updates rather than
  native IDs/codecs/cursor merge. It verifies exact ordered observations and
  detects changed neighbors, ignored deletion and missing reverse tuples. The
  paired model preserves self and parallel-edge multiplicity. It is explicitly
  a valid-input primitive model, not physical-format validation or authoritative
  directory publication proof.
- The seeded runner calls real encode/merge, constructs independent primitive
  expectations, and checks exact output. It deliberately plants ignored-delete
  and missing-reverse observations. Cancellation fires at initial/middle/final
  actual checkpoints; a real accumulated-work refusal and final reserved-byte
  corruption fire separately, followed by same-seed clean execution. Eight
  declared coverage entries are registered and the real runner routes the probe.
  The inspected final logs show four seeds with `(comparisons,fires,controls)`
  `(4,5,5)` and one seed-0 runner episode with 59 operations, zero violations,
  and all required entries. These owner runs were inspected, not rerun here.
- The bounded fuzz target drives both base and delta decoding/merge, then
  reencodes successful partitions and requires exact edge round trips. It seeds
  both literal goldens. The retained fuzz log reports 2,794,685 executions in
  61 seconds without a failure. This is a narrow parser/merge smoke, not broad
  product recovery or performance qualification. The inspected final-checks
  manifest records successful focused core/runner/oracle, lint and formatting
  checks; mutation logs record four intended failures with exit 100, and current
  source hashes match the restored reviewed bytes.

No candidate source or tracker was edited. No broad suite or fuzz campaign was
repeated. Real OUT/IN participant agreement, admitted-view sequence binding,
endpoint liveness, authoritative entity topology, actual shared-owner accounting,
publication/reopen/recovery remain with ZE-44/45/47 as originally required.
