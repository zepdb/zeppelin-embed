# ZE-44 initial native producer review

Date: 2026-09-19

## Verdict

Scoped PASS. I found no concrete correctness, transaction-composition, range-update, memory-accounting, work-accounting, or cancellation defect in the frozen initial producer checkpoint. This is a review of the initial compiled producer only, not completed ZE-44 acceptance.

## Frozen identity

- Frozen directory: `/tmp/ze44-producer-initial-review`
- Parent: `273eb33e66c281da18264d7f5eb7c08165e63ddf`
- `sha256.json` SHA-256: `a94b3fca95736709873e4efc149a92e9840a21d1bb98c5583fe3c5c7aa5c31cd`
- Initial manifest verification: `verified_entries=22 mismatches=0`
- `prepare.rs`: `464fd42dcf65d229d4ecabc5222d180cee5d3b897806f01c70981ca1584f8e88`
- `prepare/ranges.rs`: `795d94fa4f44f423702c2ee3e5a80d694be2cd1e2ceb5ae598102aaec11b78a4`

Raw identity and command results are in `/tmp/ze-44-producer-sol-review-hashes.txt`; the complete per-file frozen hashes remain in `/tmp/ze44-producer-initial-review/sha256.json`.

## Standards

PASS, zero findings in the reviewed source. Production code contains no `unwrap!`/`expect!`/indexing/panic placeholders, uncharged collection fallback, new dependency, Rayon, serde, or hidden recovery path. Fixed-capacity storage uses `StorageBuffer` and `StorageReservation`; errors remain typed `TreeError`s; the producer returns a single private candidate only after records, OUT, and IN roots have all succeeded. The generic fixed-width heap-sort widening preserves the controlled algorithm and the frozen shared-sort regression is GREEN.

## Spec

PASS, zero findings in the bounded initial checkpoint.

`prepare_native_graph` first binds the retained base, checks the real staging/memory owner, runs plain-DELETE Restrict against the admitted pre-batch roots, prepares authoritative records and indexes, then derives adjacency solely from verified old/new relationship records. It uses the merged `BatchCatalog` and final node roots for new-edge endpoint liveness. Property-only topology emits no adjacency change, relationship deletion remains legal after DETACH, missing fresh endpoints fail, tombstoned fresh endpoints are rejected, and DETACH itself never enumerates adjacency.

The candidate retains `BaseIdentity`, exact base WAL sequence, all eight expected graph-root slots, final combined roots, and either the checked target sequence or the unchanged no-op cutoff. Intermediate record-only or OUT-only success is not exposed.

Changes sort by `(direction, full NodeId u128, RelTypeId, full RelId u128)` and reject duplicate directed observations. Range selection uses the reviewed predecessor seam, bounds a true gap by the next same-group lower ID, and does no successor arithmetic. Each selected old range is fully kernel-validated before a two-way incoming merge. The preflight enforces insert absence, delete presence, and exact neighbor identity. Append stays within eight runs and 2,048 pending entries. Consolidation removes the old interval before emitting checked final bases of at most 4,096 entries, all at the one target sequence; an empty result removes the range. No over-limit descriptor is admitted.

The live simultaneous workspace charges the merge kernel state, 6,144-edge merged output, 6,144-edge old copy, 4,097-edge streaming output, 2,048 delta entries, encoded bytes, tree scratch, change array, participant candidates, and prepared artifacts through the same `StorageMemory`/writer owner. Initialization and copies are chunked at no more than 64 KiB, scans and two-way comparisons checkpoint actual work, and the final success paths call `step(0)`. On failure, local reservations drop, no candidate escapes, and the caller-owned `PreparedObjects` retains private abort inventory.

## Commands and evidence

I created detached scratch worktree `/tmp/ze44-sol-producer-review-wt` at the pinned parent, copied only the 16 owned source/test/evidence paths from the frozen directory, and used separate target `/tmp/ze44-sol-producer-target`.

```text
git diff --check
```

Result: exit 0.

```text
CARGO_TARGET_DIR=/tmp/ze44-sol-producer-target cargo check -p zeppelin-embed --lib
```

Result: exit 0; exact frozen overlay compiled successfully in 4.21 seconds.

I did not run nextest because the delegated constraint permits only an actual defect-driven focused probe, and no concrete defect hypothesis survived the static and compiled-delta review. The frozen evidence itself records:

- `green-producer-initial.log`: 6/6 focused tests passed, nextest run `6d7a76aa-fde9-4824-a0e9-35b9e419e506`.
- `green-shared-sort-regression.log`: 1/1 passed, nextest run `49db93fb-a08d-4a67-9f9f-fd7e3a70ce18`.
- `clippy-producer-initial.log`: scoped check completed successfully.
- `red-native-producer-api.log`: the intended missing producer API failed before implementation; the two intermediate compile logs are diagnostic and are not treated as intended RED evidence.

No source was mutated for a can-fire exercise, so no restoration run was required.

## Qualification boundary

This review does not qualify the still-pending 4,097-edge, ninth-run, 2,049-entry, multi-generation history, actual ZE-129 oracle, missing-reverse/ignored-delete runner, allocation-refusal, work/cancellation matrix, physical reopen, registry, public lease, coordinator, publication, WAL recovery, or broad ZE-118 obligations. Their absence from this initial checkpoint is not a finding. I did not repeat the already accepted descriptor/predecessor review or independently audit `read.rs` and WAL/base binding beyond checking how the producer calls them.

No main file, owner-worktree file, tracker state, commit, branch, or remote was modified.
