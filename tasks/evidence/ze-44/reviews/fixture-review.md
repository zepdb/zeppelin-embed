# ZE-44 large-history fixture and accounting review

Date: 2026-09-19

## Verdict

Scoped PASS. I found no remaining defect in the frozen final large-history fixture, its split-pack retention proof, or its simultaneous memory/work accounting. The initial checkpoint's under-reserved outer `Vec` replacement overlap is corrected in the final checkpoint before allocation. This verdict does not extend to the separately owned actual ZE-129/ZE-133 runner or any public lifecycle claim.

## Frozen identity

- Parent: `273eb33e66c281da18264d7f5eb7c08165e63ddf`.
- Initial checkpoint: `/tmp/ze44-threshold-checkpoint`.
- Initial `sha256.json`: `6c94511164a3072908689f6e54c8ef8eec738d5948b7013996e44e6bd999a284`.
- Initial verification before review: 26/26 manifest entries OK.
- Initial verification after final review: 26/26 manifest entries OK; manifest, owned-paths, and scope hashes remained `6c94511164a3072908689f6e54c8ef8eec738d5948b7013996e44e6bd999a284`, `d20bcd6337f6bb94cbaed641f5a49dc9adc1ec37934dc634064ce261c492f43b`, and `0e0eba18699b3f1cd66c5e84b5587c46c70933f035945cc6326b0c74972a7c69`.
- Final checkpoint: `/tmp/ze44-fixture-final-review`.
- Final `sha256.json`: `9149f0876c84610994d36018489b561a51ee2fafe7661f97a12c490d5053c209`.
- Final verification before review: 43/43 manifest entries OK.
- Final verification after review: 43/43 manifest entries OK; manifest, owned-paths, scope, and delta hashes remained `9149f0876c84610994d36018489b561a51ee2fafe7661f97a12c490d5053c209`, `f77acddb7ac4508a96d091844cf92710e484076fb969c093d25817fbf089fe24`, `eafc8bcbd85e11b9ae41a171d3bfec8bb9bb7ba0dadedd63c7d301fd85c5b4cb`, and `9f21bc76beb2be6b76781f113113776dc13262704c4455ee512b6c643a551710`.
- Final `native_adjacency.rs`: `ce544acccd30c19d66b9ab463477322e6d1fafd7bd0c6714e92a41c03cf04e82`.
- All 12 frozen production source hashes are byte-identical between the initial and final checkpoints. The reviewed correction is test/evidence only.

## Fixture source and retained physical files

The split sink in `crates/zeppelin-embed/tests/graph_storage_prepare/native_adjacency.rs:200-302` delegates every append to one of two real `PreparedObjects` instances under the same `StorageMemory`: `TreePage` blocks use the page lane and every other block uses the nonpage lane. It does not replace the producer, codec, object framing, checksums, or abort ownership. Odd/even artifact identities route reads back to the correct real packer. The lanes cannot collide: parity separates them, the 10,000-ID generation spacing separates generations, and the 32 MiB preparation ceiling with 512 KiB packs bounds a successful generation far below that spacing. `SplitPacks::finish` checks the combined exact abort inventory before sealing both real pack sets.

`reachable_page_packs` at lines 304-351 starts from each of the eight optional `GraphRoots` slots, decodes every encountered page, and follows every branch child. `retained_files` at lines 402-445 retains a complete file image for every nonpage pack and every page pack containing a page reachable from those roots. A reused old root falls through the nested `Frozen.previous` source; the recursive call keeps that earlier generation's file bytes, frames, and shared charges alive. A partly live pack stays whole. This is exactly the required test-fixture retention model and makes no production reclamation claim.

Each finalized `PreparedArtifact::bytes()` is written in full, reread to exact EOF in at-most-64-KiB chunks, and held under an exact shared reservation. After `objects` is dropped at line 667, `decode_with_control` fully validates the copied file bytes and expected store/artifact identity before constructing the only source used by the next generation. `verify_fresh_final` at lines 448-499 then checks the authoritative relationship count and literal ordered OUT and IN range contents through this fresh file-derived source. It does not fall back to the dropped private packs.

## Histories and observations

The 4,097-edge fixture is exactly 36 generations: 31 batches of 128, one batch of 125, and four batches of one (`[128; 36]` with index 31 changed to 125 and indices 32-35 changed to one). The expected total is derived from the prior committed high-water plus the fixture input count, then cross-checked against the staged batch. Every generation checks both directions, every RelId, the opposite endpoint, and all descriptor caps. The final state checks bases `[4096, 1]`, zero deltas, the full-width split boundary, and finite/infinite range bounds. The fresh final observer sees all 4,097 authoritative records and all 4,097 raw OUT plus 4,097 raw IN entries.

The 2,049-pending fixture is exactly eight generations: `300 x 6 + 248 + 1`. Its final descriptor is base 2,049, zero deltas, at the target sequence in both directions; the fresh observer sees 2,049 records and matching raw OUT/IN entries. The ninth-run fixture is nine one-edge generations and ends at base 9, zero deltas, watermark 109.

The final pinned evidence records:

- 4,097: terminal isolated GREEN `d36b52f7-ed9e-4bd9-b084-30c8d575a134`; final fresh read work 24,046,273; full shared peak 98,105,711 bytes; storage peak 11,285,696 bytes; writer retained after recursive unwind 80 bytes, which is the still-live `StorageMemory` owner.
- 2,049 pending: final fresh read work 7,338,172; full shared peak 44,968,892 bytes; storage peak 24,191,040 bytes; writer retained after unwind 80 bytes.
- Ninth run: full shared peak 2,744,812 bytes and storage peak 2,143,584 bytes.

## Accounting and work

The initial checkpoint reserved two outer descriptor copies before `Fixture::after`. That does not conservatively cover an amortized `Vec` replacement, because the old and new buffers overlap during allocation and can approach three final-length descriptor buffers. The final source at lines 684-717 reserves four complete entry and symbol descriptor sets before cloning/growing either vector, then measures the retained capacities and shrinks the charge only to the exact live backing. Four sets conservatively cover old-plus-new growth. Canonical byte clones are also included while the separately charged prior fixture and staged batches remain live.

`ExternalBytes` and `ChargedVec` place backing before reservation fields, so Rust drops the backing before releasing its charge. Each recursive frame explicitly drops `next_base` before its fixture charge; file bytes, decoded frames, staged batches, tree workspaces, and private objects otherwise release through the same scoped owners. The terminal `writer_retained=80` observations show all recursive staging/storage reservations returned except the intentionally live `StorageMemory` object. The external fixture owners are shared-only by design and cannot escape their recursive frames.

The tests retain the real 32 MiB `StorageMemory`, default 64 MiB `WriteMemory`, and 256 MiB store aggregate. Successful reservations enforce all three ceilings. The monotone `GraphResources::peak_reserved_bytes()` call at lines 729-735 includes the full simultaneous external fixture, writer, and storage overlap; it is not a sum of independent budgets.

The 200,000,000 work values are explicit per-operation fixture allowances, not a hard product contract. Ninth-run and 4,097 producer operations remain within that chosen allowance. The pending-history producer explicitly chooses 400,000,000 and measures a 264,081,021 maximum; the separate one-batch 2,049 case still proves typed `TreeError::Work` at 200,000,000 without relabeling that value as a product limit. The 200,000,000 file-copy/admission context begins only after the producer, private range validation, sealing, and producer-work capture are complete. The nonrecursive final fresh read is another completed-source operation with an explicit 1,000,000,000 allowance and reports its much smaller actual work. No producer counter is reset midway.

## Evidence handling and limits

I inspected the pinned raw runs and exact final source. I did not rerun nextest because no concrete defect hypothesis survived the source/accounting review, and the delegation requested focused scratch checks only for such a hypothesis. The original combined run is correctly recorded as 50 passes plus one generation-33 stack-overflow SIGABRT from the earlier recursive verification placement. It is not reported as green. The corrected final source moves that verification to a nonrecursive helper; its isolated 4,097 run is terminal GREEN, while the preceding 50 passes remain separate evidence.

This PASS covers only the private producer fixture, physical pack retention, exact final file-derived observations, and its accounting/work claims. It does not qualify `GraphStore` admission, a retained public lease, exclusive creation/fsync, WAL publication/recovery, coordinator behavior, public ZE-45 reads, production GC, or broad ZE-118 campaigns. The independent actual ZE-129 model adapter/runtime registry and its missing-reverse/ignored-delete controls remain ZE-133 work and are not present in this checkpoint.

No main file, owner worktree file, tracker state, commit, branch, or remote was modified.
