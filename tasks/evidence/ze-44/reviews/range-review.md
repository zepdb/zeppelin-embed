# ZE-44 frozen range component review

Date: 2026-09-19

Verdict: PASS for the frozen range component. I found no concrete correctness,
format, ownership, resource, or control defect in the reviewed bytes. This is a
bounded component verdict, not ZE-44 producer/read/public-view acceptance.

## Frozen identity

Frozen directory: `/tmp/ze44-range-review`

Manifest SHA-256:
`522b434daeff630fc6d0b66e8b278300ce54fbdf2c671668019db56dc7cbafc6`

The final manifest check read every listed file from the frozen directory and
reported `OK` for all 19 entries. Relevant production/test hashes are:

- `adjacency/mod.rs`: `cb61ae6e546702a9d261ebeadf741f9f2da11df188f141c7dd74a35c657d53ac`
- `adjacency/range.rs`: `1bb82d1f876a47a024c624a67262257cf158203fbf27514fd8e3959d4d2e7c60`
- `adjacency/range/edit.rs`: `50b7e7651d44772056a635852e892cfbfa8cf0cda29ff21bdd661799f6e5ff74`
- `tree/directory.rs`: `b63cb2aa5835792b0e0fd2cf6a3bdd76473818127f6283efb9dceca890093549`
- `graph_adjacency_store.rs`: `8cb879a468063f670f368ff4a1743932083d7219a6dca1494c2a8e1c813084ad`
- `graph_directories.rs`: `5090611184e1b29a83a9bc77dbbe360310ef6d3f0e358094621483fd8bb3253e`
- frozen interface proposal: `d849b14dccf27f5fc5d32e16e3c692648f0dab3a8839ecfe8d05112bb7939d7f`

I verified the same manifest hash before review. The owner worktree's range
source and tests matched the frozen hashes while I ran the checks below.

## Contract review

The 328-byte descriptor is canonical and fixed-width. Decode requires the exact
40-byte numeric key and 328-byte value, version 1, tree-matching direction,
zero reserved bytes, canonical finite/infinity upper bounds, at most eight
active deltas, exact pending/base caps, a required typed base reference, a
contiguous active delta prefix, and zero inactive slots. Zero-base/nonzero-run
descriptors are valid; zero-base/zero-run descriptors and zero-entry persisted
runs are rejected.

`validate_range` binds the entry to its exact root, decodes the outer shape,
resolves every base/delta through the actual `BlockSource`/`PreparedObjects`
path, rechecks exact reference/store/artifact identity and artifact generation,
and bounds child generations by `DirectoryEntry::creation_generation`. It then
uses the reviewed ZE-124 merge kernel for complete header, group, range, order,
sequence, topology, action, and entry validation before checking exact inner
base/pending counts. Empty merged persisted ranges are rejected.

New descriptors are completely resolved and kernel-validated at the target
generation/sequence before `insert_checked`. Retained values are revalidated by
the typed leaf validator, using the base cutoff for leaves at or before the base
generation and the target cutoff only for leaves created in the current private
candidate. `GraphGeneration` and WAL sequence remain distinct checked values;
`RangeEditContext` is only constructed for a changed batch, while no-op
sequence retention remains the full producer's responsibility.

`put_range` checks same-prefix predecessor and first greater successor before
page append. Adjacency equality is accepted (`finite upper <= next lower`),
finite gaps are accepted, infinity overlaps are rejected, and no successor
arithmetic is used at `u128::MAX`. Replacement, split, gap restoration, and
removal paths preserve the nonoverlap invariant; a no-survivor descriptor must
be removed rather than stored.

Resource/control handling is explicit in the real path: `RangeScratch` charges
the complete initialized 6,144-edge buffer, its own descriptor, and
`MERGE_STATE_BYTES` through the same `StorageMemory`; preparation-owner identity
is checked before validation; all initialization, physical resolution, kernel
work, count reads, interval comparisons, and final success pass through
`TreeResources` checkpoints. There is no uncharged fallback or new dependency.

## Independent commands and results

Current exact source, focused GREEN:

```text
cargo nextest run -p zeppelin-embed --test graph_adjacency_store -j 4
```

Result: exit 0, nextest run `3a756a38-2776-47d4-b81a-74daca4c1fbd`, 2 passed.

Scoped lint:

```text
cargo clippy -p zeppelin-embed --lib --test graph_adjacency_store --all-features -- -D warnings
```

Result: exit 0.

I cloned commit `273eb33e66c281da18264d7f5eb7c08165e63ddf` into
`/tmp/ze44-range-sol-probe-20260919`, copied only the five exact reviewed
source/test files needed by the focused test, verified their hashes, and ran
four one-at-a-time deliberate mutations with this command:

```text
cargo nextest run -p zeppelin-embed --test graph_adjacency_store -j 4 native_adjacency_admits_complete_private_runs_and_original_leaf_generation
```

Each mutation produced the intended RED, exit 100:

1. Bypass caller-supplied descriptor validation: run
   `b554f42f-ab7f-47cf-bbbd-8385aa8fa135`; failed at
   `new supplied value must be completely validated before insert_checked`.
2. Disable predecessor overlap rejection: run
   `8de3a694-a002-40c9-b16c-c9faa7500b6c`; failed at the predecessor-overlap
   assertion.
3. Disable successor overlap rejection: run
   `974999d4-75c0-4200-a8ca-f554acea0217`; failed at the successor-overlap
   assertion.
4. Use root generation instead of containing-leaf generation: run
   `f3ca3a0c-fd8b-48c6-a281-09302ab18997`; failed at
   `new root generation must not bless future descendants of an old leaf`.

A scratch-only probe added a 500-unit work limit and a source wrapper that
cancelled on the second actual base/delta resolve. The same focused test passed
only because it observed `TreeError::Work`, then `TreeError::Control`, with
exactly two resolution calls. Runs:

- cancellation-only probe: `91a60ab8-0faf-450a-b8c3-469b86eb64ef`, exit 0;
- work plus cancellation probe: `b0864ffe-debd-40fc-acee-dacc94ae8c32`, exit 0.

I restored every scratch mutation from the frozen-equivalent owner bytes. The
terminal scratch run `6e0c76a3-bbdd-430e-b5ec-1575f130ffd4` passed both tests,
and all five copied files returned to their frozen SHA-256 values.

Final frozen-directory verification command:

```text
python3 -c 'import hashlib,json,pathlib; root=pathlib.Path("/tmp/ze44-range-review"); m=root/"sha256.json"; d=json.loads(m.read_text()); ...'
```

Result: exit 0; manifest hash unchanged; all 19 entries `OK`.

## Authorized post-freeze prose delta

During review, the interface proposal was the only frozen-manifest path that
changed in the moving owner worktree, from frozen hash `d849b14d...` to
`c30d568b...`. Other concurrent producer work was outside this frozen review.
The exact proposal delta adds one 11-line approved clarification: after
validating old kernel
output, the producer may combine it with sorted incoming changes in one bounded
two-way pass and emit final bases of at most 4,096 entries at the single target
WAL sequence; it must admit no over-limit intermediate descriptor, remove the
old interval before disjoint replacements, discard the candidate on failure,
treat sub-1MiB scratch as unmeasured, and reuse ZE-43's controlled fixed-width
`Ord` heap sort. This moving prose was not silently treated as frozen evidence
and does not alter any reviewed range source/test byte.

## Qualification boundary

I did not re-review the already accepted predecessor implementation beyond
confirming its frozen hash. I did not run broad workspace, coverage, fuzz,
fault, threshold, or public lifecycle suites. The full staged producer, actual
OUT/IN/authoritative-record agreement, 4,097/ninth-run/2,049-entry paths,
same-batch endpoint/catalog behavior, missing-endpoint corruption versus
tombstone hiding, real read/degree/plain-delete paths, all-or-none candidate,
runner mutants, WAL admission/publication/recovery, and public lease remain
open ZE-44/ZE-45/ZE-118 work. No main, owner-worktree, tracker, dependency, or
repository evidence file was edited by this review.
