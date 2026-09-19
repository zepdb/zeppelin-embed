# ZE-43 bounded admitted-reuse review

Disposition: **no remaining blocker** in the frozen correction plus the two reviewed immutable test deltas. Both evidence precision findings below are resolved. Production bytes are unchanged from the 40-file snapshot. No moving source/worktree, main, tracker, or test campaign was modified or rerun by this reviewer.

## Exact reviewed source

Snapshot `/tmp/ze43-reuse-review`; all 40 manifest entries independently SHA-256 verified, including a terminal recheck after both test deltas. Manifest SHA-256 `cdb624fdcf745fb04c794108e537ad7f7700de15e54bf3001250f352f1480ae4`; supplied exact diff SHA-256 `bc8fe32de98a694f339e9183aae47d78c39f177908103279e1e3500c52380a75`. Actual comparison with `/tmp/ze43-pg8-review` found exactly the eight changed paths represented by that diff and two newly included fuzz correspondence files. The behavioral production change is only these three files:

| File below `crates/zeppelin-embed/src/property_graph/storage/` | SHA-256 |
| --- | --- |
| `artifact/private.rs` | `6315bb2cbe05aa714105730942f2cf70a7619cc521713ae86db3e4210c03cf12` |
| `payload.rs` | `da749f94b0b54c36acfdb9de04af96a96b8063776aefaac8df6eed2f88c06350` |
| `stream.rs` | `942375b98ce0d375f70a6a887e3a79563fb2af8c3ec424056ba61e5ffd254949` |

`prepared.rs` at `44ff1889e8e467043eb786228e0627ebeb07cf671c32e102be963d0de967c628` is the previously accepted identity-only callback documentation correction. Full per-file hashes and inspected evidence hashes are in `/tmp/ze43-reuse-independent-checks.json`.

## Production assessment

- `VerifiedEntry` is private and has one construction site. The caller first calculates the payload checksum, marks the pack failed before mutation, copies the header/body in controlled chunks, and completes the shared `validate_block` header/flags/length/checksum validation. Only then does it charge and append the entry, perform a final checkpoint, and reopen the pack. A refusal before full admission cannot create a reusable successful entry; a refusal after mutation leaves the whole private pack failed.
- Cached resolution retains the exact preparation-owner identity check, rejects failed state, checks artifact identity, performs a charged bounded binary search, and compares the entire `PhysicalRef` before taking a checked extent. Offset alone never authorizes access. Each comparison and successful return polls cancellation; no hash-sized work is claimed for work no longer performed.
- The admitted bytes remain immutable. Append only extends the body. Seal appends the directory/trailer and writes the file header only in bytes 0..96, before all block offsets. No public mutable slice is exposed, and borrowing prevents mutation while a block slice is held. Seal still performs complete shared decode/hash validation. Fresh physical-file admission remains byte-identical to the previous reviewed implementation.
- Public `PayloadRef::span_at` still selects the original 64 KiB maximum. The new internal helper clips only the final borrowed/charged window after the same root, physical reference, store/generation, extent descriptor and chunk-geometry checks. Logical bounds remain at most 32 MiB. `PayloadSlice` computes the remaining checked window, so no byte outside that window is exposed. Copies, scalar cursor reads, comparisons and UTF-8 validation retain their own bounded byte charges and checkpoints. No budget, limit, allocation ownership or external owner has been replaced with a shadow allowance.

## Focused evidence inspected

These are owner-run results inspected by this reviewer, not independent executions:

- Literal RED run `75647b65-6e45-473e-8de8-baff4d90154d`: both new tests failed as intended under prior production; the eight-byte field charged 65,546 work, and repeated exact private lookup failed the no-rehash work gate.
- GREEN run `e5f0ab34-caa5-40f9-94e6-7b38ed40b0cf`: all four directed tests passed, including the exact-ref zero-allocation check under `allocation-audit`, failed append/seal handling and original 2,048-label/2,048-property fixture. That fixture records 71,705 canonical bytes, 65,680 native bytes, 879,696 participant peak bytes and 54,075,056 work under the unchanged 200,000,000 allowance. It checks full canonical bytes, sorted labels and every property result, then released owner reservations.
- PG8 RED run `e84dc19d-66a5-44f6-8537-832ccb54a438` failed the actual repeated private-read work assertion with prior rehashing behavior. GREEN run `2ab48ab8-39f9-4b49-bdef-ecd97742de25` passed the probe and canonical `run_program` test. The source registers the required key in both registries and verifies every required key through the actual runner. Coverage is emitted after a completed measured run, not merely after scheduling it.
- Affected storage run `dc3e5626-2277-44b5-aed8-1ec41cbf4b04`: 58/58 passed across the four storage binaries using `allocation-audit,test-support`. Both strict scoped lint logs finish successfully. These include the already reviewed cancellation/fresh-file corruption cases; this review did not repeat their wider source review.

Exact supplied commands are the four commands in the owner's final-checks record: focused nextest over graph_artifact, graph_directories, graph_storage_prepare and graph_storage_failures with allocation-audit/test-support; the two named PG8 tests; matching strict core and adversarial-test Clippy. Raw evidence is independently pinned/copied under `/tmp/ze43-reuse-reviewed-evidence`.

## Review findings and disposition

1. Cleared evidence gap: original scope prose claimed refusal of an earlier admitted entry in the same subsequently failed pack, but the original assertion used a reference obtained from a separate clean pack. It did correctly use ample original resources, and proved failed-candidate refusal. Owner's immutable one-file replacement `/tmp/ze43-reuse-final-test-delta` now first successfully appends/resolves a small entry, then fails a second append at two mutating work cutoffs and checks the original reference returns `TreeError::Invalid` under newly created ample same-owner resources. Replacement SHA-256 `740bb95e456651e02892e070419f36418621fbd7037f0b75e62f1c6aa186735f` verified. Owner narrow GREEN run `3c7d1cf2-3132-492e-902c-ee5b68c8a9fa` passed 1/1; strict test lint also passed. No production delta.

2. Cleared evidence attribution: original PG8 code always selects the Nodes root. Relationship-only generations 3 and 6 retain that root from the prior generation, so those reads can fall through to previously reopened `OwnedArtifact`; they are not eight separate private-generation receipts. Other node-mutating generations do exercise actual private reuse, including the literal RED above. The final immutable replacement `/tmp/ze43-reuse-final-pg8-delta/tests/adversarial/graph_directories.rs`, SHA-256 `35dbed7481c3c805c637ef77777165b410ace4efa6bcc3af9221819c753bfca6`, selects `KeyFences` and explicitly asserts the reference artifact belongs to the current private `abort_inventory` before measured reads and credit. Every fixture operation changes a keyed fence. `PreparedObjects::resolve` checks these current pack identities before the base source, so this assertion pins all eight generation receipts to actual `PrivateArtifact` reuse. Owner observed the new membership assertion fail with Nodes (RED run `80150ba6-8ba9-4744-b112-5b7d8a325926`, expected private-backing message), then both directed probe and canonical runner passed with KeyFences (GREEN run `34a60bac-57ed-4d85-9560-2028062bfa0f`, 2/2), followed by strict scoped lint. All logs were inspected and independently hash-pinned. No product correctness bug or production change was involved.

Final test-delta manifest hashes: same-pack test `d49c4038894560501b5ca8da6f71cfe3778d49d3d39476d09d43fad64fbaac14`; PG8 route `6057e0b214a3c6dc3383a7ea5dbc39fd73567eceb64d11952aa7305133e069cf`. All entries in both were independently verified; the effective reviewed source map is the original 40 entries with only these two test-file overrides. The 58-test run precedes these test-only strengthenings; the changed failure test and both changed PG8 paths have their own terminal GREEN/lint receipts. No full-suite repeat is claimed or needed for this disposition.

## Qualification boundary

This is a bounded review of the admitted-reference/window correction and its new tests/PG8 receipt. It does not repeat accepted COW, ownership or PG8 history/oracle reviews; does not review the newly included fuzz files as a new parser campaign; and does not confer GraphStore admission, writes publication/recovery, lease, durability or broad-suite acceptance. Existing component ownership obligations remain unchanged. Nonessential broad qualification remains ZE-118.
