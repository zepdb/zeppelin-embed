# ZE-43 corrected COW independent review

## Disposition

**The first review's COW corruption-validation finding is resolved in this frozen snapshot. No open concrete blocker remains from this bounded review.** This is source/preparation acceptance, not public GraphStore/lease/WAL recovery or final ZE-43 acceptance.

Snapshot: `/tmp/ze43-cow-corrected-review`, based on `81e6e955baa987e0c3dae171b7863174d8686a3e`. Independently checked all **28** manifest hashes on arrival and again after validation. Every restored scratch file also matches its frozen hash. The original three observed REDs and report remain at `/tmp/ze43-directory-independent-review.md` and `/tmp/ze43-directory-independent/`; retain them with this follow-up in final evidence.

## Correction reviewed

- `DirectoryMutation` requires a `LeafValidator` on the checked insert, remove and typed fence mutation seams. `find_path` validates **every** entry on the original touched leaf, including an entry that will be overwritten/deleted, before constructing replacement pages. The `DirectoryEntry` retains its actual leaf creation generation and original root; `require_root` rejects context substitution.
- `NativeDirectoryValues` interprets complete node live/tombstone and relationship records with `PayloadSlice` bounded by that original leaf generation, not the new target generation. The existing native/provenance/canonical validators remain responsible for full topology, revision, property/label and provenance correlation. Fence validation similarly receives the actual old leaf generation and root. Membership validators require the proper label/type role and empty value; inventory uses its full descriptor/state verifier for all entries on the touched leaf.
- `find_path` visits every immediate child of each parent that will be copied. The child resolver is bounded by the old parent's generation; `checked_page` applies inherited lower/upper bounds and its own artifact/page creation generation agreement; the added level check requires exactly one level below the parent. This runs even for a child outside the edited route and before the first directory append. It does not recursively scan unchanged subtrees whose containing pages remain immutable.
- The typed fence seam preflights the old path before preparing any long new key extents. Normal insertion then rechecks the required path before page copying. Remove uses the same preflight. Existing root-collapse checks retain original parent bounds for untouched survivors.
- Production callsite inventory in the exact snapshot contains only checked mutations: `participant.rs` uses `insert_checked`/`remove_checked` with `NativeDirectoryValues`, `insert_fence_checked` with the same owning validator, and checked membership updates with `MembershipValues`; `inventory.rs` uses `insert_checked` with its private `InventoryValues`. No raw-primitive production bypass was found. Raw public primitive fixture APIs still deliberately use opaque values, but they invoke the checked path with `OpaqueValues`, so branch structure/generation checks also remain mandatory there. Raw opaque leaf values carry no admission claim.

The no-write assertions specifically prove **no replacement directory append** for an invalid original leaf/branch. A full participant may have prepared private provenance/canonical payloads earlier; those remain its abort-inventory responsibility on error. The correction does not claim physical rollback of all prior private writes.

## Independent verification

Built a separate `/tmp/ze43-cow-independent/repro` from `git archive 81e6e95`, overlaid the exact 28 files, and used `/tmp/ze43-directory-independent/target` as the isolated target directory. No real worktree or immutable snapshot was edited.

Focused nextest, four test processes, default features: **11 passed, 0 failed, 22 filtered**, exit 0. This includes the three original finding reproductions now asserting rejection before writes, node and relationship full-value controls, the root-collapse malformed-level control, the two default-feature preparation tests, and three private-artifact failure/reopen tests. The feature-gated allocation-audit test was not run by this reviewer. Exact command and raw output are `/tmp/ze43-cow-independent/final-receipt.json` and `/tmp/ze43-cow-independent/terminal-green.log`.

Two deliberate regressions were planted only in the isolated scratch source:

1. Bypass the old leaf validator: the fence, inventory, node and relationship tests all fail at the intended original-value validation assertions, **0/4, exit 100**. Raw log `skip_old_leaf_validator.log`.
2. Bypass the old parent's immediate-child checks: the future unselected child test fails at the expected COW-rejection assertion, **0/1, exit 100**. Raw log `skip_old_parent_child_checks.log`.

Both mutants were restored byte-exactly. The final **11/11 GREEN** run occurred after restoration. Mutation commands, outcomes, log hashes and restored source hash are in `/tmp/ze43-cow-independent/independent-mutations.json`; the final receipt independently re-verifies all 28 source/test hashes against the frozen manifest.

## Related preparation evidence reviewed

The included five-generation trace writes and reopens each generation's prepared artifacts into fresh charged `OwnedArtifact` buffers, drops all original packs, then reads both earlier and later roots. It checks original provenance and old stored text after reopening. This closes the earlier same-preparation-only limitation for that component fixture; it still does not exercise public recovery, durability, or a reachability/inventory tracer.

`OwnedArtifact::read_from` now polls between physical read attempts, including Interrupted returns, instead of delegating unobservable retries to `read_exact`. The independently passing directed test checks a 65,536-byte maximum read request, exact EOF, cancellation before the next retry I/O and released ownership on failure. The other selected tests cover malformed/truncated/trailing/wrong-store admission and private append/seal/finish failures preserving abort inventory without finalized-artifact escape.

The multilevel directory test now grants each separate immutable edit its own 10M work context; it no longer tries to spend one aggregate 2B context across 1,700 independent edits. No production budget constant or limit was raised by that test-only adjustment. The owner's recorded full 55-test focused run and strict scoped clippy remain separate owner evidence; this reviewer did not repeat those broader selected targets.

PG8 integration, parser fuzz and final ticket packaging remain the storage owner's active work. Nonessential broad qualification stays ZE-118. No tracker mutations, commits, moving production edits, or broad campaigns were performed for this review.
