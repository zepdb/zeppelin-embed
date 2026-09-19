# ZE-43 independent directory-participant review

Review is against the immutable 26-file snapshot `/tmp/ze43-directory-participant-review`, based on `81e6e955baa987e0c3dae171b7863174d8686a3e`. All 26 SHA-256 values in `hashes.json` were independently verified, including again after the probes. Prior preparation review: `/tmp/ze43-preparation-review/independent-review.md`.

## Disposition

**One blocking corruption-validation finding, demonstrated in three focused reproductions.** The storage owner has acknowledged the mechanism and is correcting it. This report does not accept the pending correction or claim terminal GREEN. No additional concrete production blocker was found in this bounded review.

## Finding: COW can legitimize a previously invalid future reference

The new `DirectoryEntry::creation_generation()` protects values when an old physical leaf is merely presented beneath a newer root. However, a COW write copies unmodified leaf values or branch child references into a newly dated page. Without validating each copied reference under its original containing page generation first, the newer page raises the reference's effective bound. A checksum-valid invalid reference can consequently become accepted through an unrelated write.

Relevant frozen source:

- `storage/tree/directory.rs:327` (`checked_page`) validates page geometry and keys but leaves values opaque; `insert_key` beginning at line 856 copies untouched leaf cells, then emits a page with the requested newer generation.
- The same insertion propagates untouched branch children into a newer parent. `find_path` bounds the selected child by its original parent; an unselected sibling is not visited before being copied.
- `storage/participant.rs` snapshots and validates the entity directly changed, but does not validate all retained semantic values in its touched leaf before mutation. `records/fence.rs:79` correctly validates against the returned containing-leaf generation, which has already widened after COW.
- `storage/inventory.rs:76` checks preexisting entries only for the changed artifact IDs; an unchanged descriptor in the same rewritten leaf can be carried forward without its old-generation validation.

The raw byte directory primitive deliberately permits opaque values, so fixing only its callers' new values is insufficient. The production mutation seams must validate retained values under the original leaf's bound, and COW branch propagation must validate retained immediate children under the original parent bound. This can be limited to touched pages; this finding does not ask for a full-tree sweep on each write. Any raw primitive that intentionally remains outside semantic admission needs that boundary kept explicit.

### Independent reproductions

All probes were added only to `/tmp/ze43-directory-independent/repro`, created from `git archive 81e6e95` and the exact 26 frozen files. The original snapshot and actual worktree were never edited. The deliberately malformed objects are constructed through the actual page/artifact codecs and have valid framing/checksums.

1. **Permanent fence:** an existing gen2 leaf points to a gen3 provenance artifact. `verify_fence_entry` rejects it before the write, including through a lifted root. Insert an unrelated, fully valid fence with its own matching key/provenance/canonical contents at gen4. The added fence verifies; the untouched invalid fence now also verifies because its leaf reports gen4. The assertion that it remain invalid fails. Log: `/tmp/ze43-directory-independent/cow-future-reference-valid-neighbor.log`.
2. **Inventory:** a gen9 leaf carries an otherwise valid immutable artifact descriptor dated gen10. `verify_inventory_entry` rejects before mutation. `apply_inventory` adds a different valid descriptor at gen11. The untouched bad descriptor now verifies. Log: `/tmp/ze43-directory-independent/cow-inventory-future-reference.log`.
3. **Branch child:** a gen2 parent points to a valid gen1 left leaf and a future gen3 right leaf. A root upper bound of gen4 does not override the parent's gen2 bound, so right lookup initially rejects. Updating the left leaf at gen4 copies the right reference into a gen4 parent; right lookup then succeeds. Log: `/tmp/ze43-directory-independent/cow-branch-future-reference.log`.

All three targeted nextest commands completed with **exit 100, 0 passed / 1 failed**, at their intended post-COW assertions. The first fence experiment used an unrelated opaque value; it was superseded by the fully valid neighbor fixture and is not the credited reproduction. Exact commands, architecture/OS, file hashes and logs are in `/tmp/ze43-directory-independent/review-receipt.json`. The test-only patch is `/tmp/ze43-directory-independent/independent-probes.patch`. No production mutation was planted, so production restoration was not required.

## Reviewed boundaries retained

- `prepare_directories` requires the same authentic `WriteMemory` owner as all three `StagedBatch` arenas, plus the exact retained base identity/catalog/root metadata. Numeric identity alone does not bypass the writer-owner check.
- The candidate is a non-publishable native-directory preparation result. OUT/IN remain unchanged for ZE-44; the coordinator still owns admission, base retention, abort inventory, whole publication and recovery.
- Fence bytes preserve kind-scoped application keys, exact provenance, installed revision/incarnation and original changed generation. Deletion retains permanent fence history and omits live canonical content. Full provenance/canonical checks and exact namespace/key comparisons are present.
- DETACH prepares a node tombstone instead of scanning incident edges. Raw relationship records remain; public both-endpoint visibility belongs to the later integrated owner.
- The sequential provenance writer uses charged bounded buffers, propagates the original typed storage failure, verifies total length, and polls during bounded copies and at final completion.
- Inventory retains the existing 88-byte descriptor/state layout, validates immutable descriptor equality for directly changed IDs, and does not grant reachability or unlink authority. Newly emitted inventory pages remain protected by prepared/WAL descriptors until a later fold, rather than claiming recursive inclusion in their own tree.
- Cancellation and typed-error paths are preserved by the reviewed interfaces. Nothing in this review treats private test catalog/admission fixtures as actual GraphStore or live lease/WAL acceptance.

## Evidence limits and next review

The snapshot owner reports 50 focused tests and strict scoped clippy GREEN plus three restored RED controls. Those are owner-recorded results; this review independently ran only the three additional targeted probes above. Prior preparation review's directed private-wire failure/cancellation/fresh-buffer reopen qualification and current PG8/fuzz completion remain the existing ZE-43 work, not newly invented broad gates. Broad nonessential suites remain ZE-118.

The next source snapshot must show the retained-entry/branch fix, literal intended RED followed by GREEN, and exact mutant restoration as applicable. Recheck these three specific reproductions through the production mutation seams. No code, tracker, real worktree, or immutable snapshot files were changed by this review.
