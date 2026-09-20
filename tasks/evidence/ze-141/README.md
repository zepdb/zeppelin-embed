# ZE-141 private native-to-C result conversion evidence

## Result

ZE-141 adds one private conversion seam from `ResultSource` through the
authentic `PreparedGraphResult` into the existing registered C arena under the
same `RuntimeContext`. It maps all 12 native pools directly into the final 14 C
pools with no temporary C vectors. The actual driver then supplies the final
counter snapshot to an infallible finalizer that writes exactly 23 global work
rows: 22 runtime counters plus peak owned bytes. Each search report retains its
own disjoint 22-row work range.

The public ABI, generated header, core production code, coordinator, producer,
and exported entry points are unchanged. The opt-in adversarial bridge exists
only under `graph-result-test-support`.

## Source authority and preservation

- Base and worktree HEAD before the ZE-141 commit:
  `e2d16b106593050711bc055d46d5673af0848bd7`.
- The exact Astra plan is [plan.md](plan.md), SHA256
  `85d7708488e50193b7c6fd8f4aaa976de23045cebb26ba71665ebe91fac1ad91`.
- The follow-up [spec review](spec-review.md), SHA256
  `b2f54cf97f1c3d1137b6293e4a06bb28f202944c0c9a5b064c6c4e1bf6dc7c79`,
  and [standards review](standards-review.md), SHA256
  `7a00e73789add1cfc6cdb13a1785b6c5829f3c52e590c94937020321be31037a`,
  are preserved verbatim. All findings were accepted and remediated.
- [source-before-sha256.json](source-before-sha256.json), SHA256
  `05276a143af6ee57ea2e2cae8e300e2a4d50ccb4caa163ecf17f8e8743c9d15c`,
  matched all 17 pinned paths before edits.
- [preservation-manifest.json](preservation-manifest.json), SHA256
  `1910f7df16aa70948c270e45e399c4b2c7187d52c04d3f637ff8eef682c18c34`,
  matched all 45 inherited entries before edits and again before commit; see
  [preservation-verification.json](preservation-verification.json).
- Host, toolchain, branch, features, and platform partition are in
  [provenance.txt](provenance.txt). This is local Apple-silicon macOS evidence.

## RED to GREEN

The initial compile RED was the missing private `conversion` module. After the
typed skeleton compiled, setting the node ID high half to zero produced the
intended field-oracle RED: observed `(0, 7)`, expected `(1, 7)`. Restoration
then made the complete all-pools test GREEN. Raw early logs are under `raw/`.

Harness-only REDs are retained separately. The first driver fixture exceeded
the seeded `SearchInvocations` limit, and the first overlap search stayed below
the real 64 KiB validation scratch floor. They were fixture mistakes, not
semantic evidence. The corrected overlap fixture uses more than 64 KiB of
charged source backing and tightens the measured simultaneous peak by exactly
one byte.

Fifteen deliberate semantic mutants were then killed and restored. They cover
full-width IDs, IEEE bits, explicit presence, the stored Empty list tag,
receipt disposition, report work ranges, CompletedAbiBytes, peak bytes, copied
work, C capacity accounting, the final post-registration checkpoint, allocator
silence during finalization, the canonical native runner route, both real C
allocation sites, and the source/native memory-refusal pairs. The mutation
harness saves each original once while applying later replacements to the
current mutated bytes. Mutation 03 therefore composes both presence edits and
kills both the all-pools and report tests. Mutation 09 removes the shared C
chunk charge and kills both the known-byte-delta and C-stage work-refusal
assertions. [mutations.json](mutations.json) records each replacement, command,
RED/GREEN exit status, and exact before/mutant/restored SHA256. Every RED exited
100, every restored GREEN exited 0, and all 15 restorations matched their
pre-mutation hashes. The retained-`Vec` finalizer mutant aborted only its
isolated nextest process on the denied one-byte allocation.

## Measured fixture ledger

The synthetic bounded fixtures measured the following exact local values; raw
output is in [measurements.log](measurements.log).

- Empty-result driver fixture: native represented bytes 504, C represented
  bytes 552, peak query bytes 165,400, final CopiedBytes 575, CompletedBytes
  511, and CompletedAbiBytes 561.
- Complete geometry fixture: padded C arena 1,688 bytes, real arena plus node
  allocation 2,032 bytes, conservative total C reservation 2,608 bytes, and
  peak query bytes 165,400.
- Query overlap fixture: measured required peak 223,128 bytes; 223,127 bytes
  produced the typed C-stage memory refusal and exact refund.
- Aggregate refusal fixture: shared baseline 76,424 bytes, conversion peak
  increment 146,696 bytes, unrelated held reservation 3,971,185 bytes, and
  aggregate limit 4,194,304 bytes. Releasing the hold restored clean success.
- Actual System allocator inventory: five attempted allocations, five
  successful allocations/frees, zero live bytes, peak 132,041 bytes. The two
  C allocation ordinals were separately fired with exact scoped receipts.
- Thirty-two expose/free plus 32 prepare/abort loops performed 320 allocations
  and 320 frees with zero live bytes and peak 34,192 bytes.
- Empty preparation has five retained-view checkpoints; removing the final
  post-registration checkpoint changes that exact ordinal to four and is
  caught. The large-fixture sweep covers all 652 observed retained-view polls,
  a 71,680-byte source payload, 631 C node descriptors occupying more than
  64 KiB, and the exact CopiedBytes prefixes `{0, 65536, 71680, 132256,
  197792, 203936, 269456, 269560, 270112}`. It separately proves native and C
  copied-work refusal, both final completed-byte charges, deadline timeout, and
  final close-over-cancel cleanup after registration.
- Typed rejection tests preserve Missing, Deleted, Storage, a malformed byte
  range, a self-referential list cycle, an invalid cell, committed-generation and
  vector-report contradictions, prior work, and real close-over-cancel
  precedence with zero owner exposure and exact refunds.
- Each directed PG16 seed `0`, `1`, `141`, and `u64::MAX` performs 15 native
  cases, fires seven real paired faults, completes seven byte-identical clean
  controls, and reaches all eight additive native coverage keys. Source-owner
  memory refuses at limit 76,382 before source observation; native-copy memory
  refuses at limit 242,271 after one source observation and an exact 80-byte
  copied prefix. The full PG16 feature inventory remains exactly 20 keys.

The compiled C reservation is one external guard plus padded arena, registry
node, common prepared-response/layout/metadata controls, the fixed native
initializer descriptor, and the maximum wrapper delta. The geometry fixture's
2,608-byte reservation exceeds its 2,032 bytes of real arena/node allocation;
the accounting-removal mutant makes that assertion fail. The follow-up
standards fixes keep chunk bounds, charging and pointer writes in one private
`AlignedArena::write_chunks` primitive and replace positional 14-element pool
geometry with the private named `PoolCounts` type; no public interface changed.

## Terminal focused verification

All commands used nextest profile `default`, four jobs, zero retries, and the
repository's configured single libtest thread.

- Native FFI tests, graph-only: 7 passed, 27 skipped; the follow-up run includes
  the complete 652-checkpoint sweep and typed rejection/context cases.
- Native FFI tests with allocation hook: 7 passed, 27 skipped.
- Native direct probe, canonical runner, and active-key gate: 3 passed, 472
  skipped initially; the follow-up direct probe and canonical runner rerun passed
  2 tests with 473 skipped.
- Independent graph-response oracle: 1 passed, 95 skipped.
- Hook-absence boundary: 1 passed, 470 skipped.
- Existing and new FFI graph-result regression: 20 passed, 14 skipped.
- Core completed-result and compiled-context regression: 19 passed.
- Existing PG15/PG16 direct and runner controls: 4 passed, 471 skipped, including
  a follow-up rerun after the adversarial changes.
- Frozen graph contract/layout/header regression: 21 passed.
- Strict no-dependency Clippy passed for the FFI, workspace adversarial test,
  and independent oracle targets.
- FFI no-default and graph-only library checks passed.
- Targeted format check and `git diff --check` passed.

Terminal logs and their hashes are listed in `log-sha256.txt`. No broad
workspace suite, full adversarial campaign, coverage campaign, sanitizer,
benchmark, fuzz, size, minimum-OS, Intel, Windows, or release-artifact gate was
run.

## Retained acceptance boundaries

ZE-53 retains the authentic admitted native producer and complete query
execution. ZE-68 retains coordinator/precommit consumption, outcome
reconciliation, registry policy, and the real commit/delivery allocation
window. ZE-69 retains public request/handle/error/export/free integration.
ZE-107 retains shipping artifacts and platform qualification. ZE-118 retains
the broad frozen-source workspace, adversarial, coverage, sanitizer, size, and
platform qualification for the eventual integrated commit.
