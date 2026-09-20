# ZE-135 query storage resource bridge

Date: 2026-09-19

## Result

Native tree, adjacency, node-record and payload reads now have a query mode
which borrows one authentic `RuntimeContext`. Its 256 KiB tree workspace,
fixed range scratch and all retained capacity are nested under that context's
`QueryMemory` and the same `GraphResources`. Typed lookup, scan, physical and
merged adjacency-entry, and copied-byte events charge the context's cumulative
`WorkKind` counters at the operation sites.

The node-record helper remains private and source-bound. It follows
`lookup_entry -> PayloadRef -> PayloadSlice -> verify_node_state`, returns
`Option<NodeRecordState<'source, S>>`, and does not admit a view, open a file,
or create a memory or lease owner.

This component does not implement or qualify public `GraphReadView` admission,
source/view association, protected lazy files, native close lifecycle,
coordinator/WAL/recovery/GC, retrieval, or public snapshots. Those remain with
ZE-45, ZE-39, ZE-40, ZE-46, ZE-60 and ZE-61. The ZE-44/ZE-133 producer gate
also remains independent.

## Exact source

- branch: `codex/ze-135-query-storage`
- prerequisite parent: `3c2fba32e72a03efdfa50f9ad7a51cdd2e8bcd72`
- prerequisite parent of parent:
  `1209c45bbf5f61905c616a97113b00c54b3b62e8`
- prerequisite subject:
  `ZE-44: Pin compiled storage for the query resource bridge`
- prerequisite delta: 144 paths: the 142-path frozen ZE-44 candidate plus the
  two exact ZE-131 Cargo/feature-inventory registrations
- inherited preservation manifest:
  `/tmp/ze-135-preservation.json`, SHA-256
  `ffd8d75956ba79cef43b5734a704bccc199f4d9c862ce7a831b7bd9d7c0e5bf7`
- preservation recheck: 45 entries checked, 0 mismatches
- final 16-file source manifest: `source-sha256.json`, SHA-256
  `b73f5df0a6423082c9e6733c5d848161159698a7e0035667d68209d1ef4be563`

The canonical main-worktree plan inputs read for this component were:

| Input | SHA-256 |
| --- | --- |
| `docs/graph/plans/storage.md` | `7b2c8e33fa38eb51915dcb95e57dcf784834b804fd7d98cc710ed10b105212e3` |
| `docs/graph/plans/parallel-contracts.md` | `d90bc768c03132d08318e8edc4121ba1bf5d5f3cd5287ad50dfcfda2f26bdb2e` |
| `docs/graph/plans/execution.md` | `45bb6d47b6c7c8147e82bb1ddd95ea93aaca886047374b8b8b63537f50a76787` |
| `docs/graph/plans/qualification.md` | `6890b3912fd374f1a18fb6271a6d975f42299669764b0870a295a5c53c5c7048` |
| `tasks/evidence/ze-134/README.md` | `0424edaef0a1de4c0fc87a7f8b55781e538d14bd84312489de1d0398d8c377b4` |
| `/tmp/ze-135-seam-sol-review.md` | `466c31dc90e0b0e22aa1c8b28c3f8a0410fbe264f807c929a441519dffe2a94f` |
| `/tmp/ze-135-implementation-sol-review.md` | `6fa36fea3c5bcd050ada3b1c289ac0f705e155f8e09a2cdf8c29ac29044bbc4b` |
| `/tmp/ze-135-astra-scratch-plan.md` | `eccf632eac403c97487dd6281ac92ab57b85217557184152960bdd1ad0673b9c` |
| `/tmp/ze-135-standards-review.md` | `db034aeaa550a886d0961dff810987fa97b833df27319182a3053e463c347dde` |
| `/tmp/ze-135-spec-review.md` | `2d35f82d7e3de1c2f282d28a38bcfd093728c7e8f9346eb279a6fa215f3784d6` |

All target-dependent byte measurements were taken on macOS 27.0 build
26A5388g, Darwin 27.0.0 arm64, on a MacBook Pro Mac15,9 with an Apple M3 Max
(16 cores: 12 performance and 4 efficiency) and 128 GB memory. The target was
`aarch64-apple-darwin` with rustc 1.93.0 and LLVM 21.1.8; Cargo was 1.93.0.
The raw command output is `raw/review-hardware-target.log`. In particular, the
197,992-byte `RangeScratch` measurement is specific to this target and build.

## Ownership and errors

- `TreeResources::for_query` calls the retained context checkpoint before
  admission, reserves exactly 256 KiB through `context.memory()`, and keeps the
  originating context borrow for its lifetime. It introduces no scalar query
  work allowance.
- Query `RangeScratch` begins with a fresh context checkpoint. It reserves one
  full `QueryArena<Edge>` for 6,144 edges and separately reserves only the
  outer descriptor and `MERGE_STATE_BYTES`. The outer charge subtracts the
  already charged embedded `QueryArena` descriptor. On this target the
  independently computed total scratch charge is 197,992 bytes.
- Directory cursors retain the exact `QueryMemory` and originating runtime
  identity. Both `next` and `next_entry` reject a same-memory, different-runtime
  resume before charging the substitute context and latch the cursor error.
- Query scratch retains and checks both owner identities at use. Its lifetime
  remains coupled to the originating runtime borrow so safe code cannot destroy
  that runtime and reuse its address while the scratch remains live.
- Query rejections retain `TreeError::Runtime(RuntimeError::{Limit, Value,
  Memory})`. Close remains first inside `RuntimeContext::checkpoint`; no raw
  `QueryControl` fallback was added.
- All constructor and read errors release their tree/scratch reservations.
  Refused copies leave the corresponding caller output slot unchanged and do
  not charge the rejected event.

The query cap remains 24 MiB. The preparation path retains its 32 MiB
`StorageMemory`, the writer retains 64 MiB, and shared graph accounting retains
256 MiB. The existing `for_prepare` tests and outputs pass after the necessary
enum/layout changes.

## Exact event accounting

The existing `TreeResources::step` diagnostic sum is unchanged and is never
reinterpreted as a semantic counter.

- `Lookups`: charged once on entry to the common directory lookup, including a
  missing lookup.
- `Scans`: charged once when `DirectoryCursor::seek` enters a source scan,
  including an empty root.
- `AdjacencyEntries`: charged for every codec `Work::EntryBytes` callback and
  separately for every later merged `Edge` examined before relationship-range,
  authoritative-record, and endpoint-liveness filters. Header bytes and
  comparisons are excluded.
- `CopiedBytes`: charged only after source and target bounds and the diagnostic
  step pass, as the last fallible action before the actual infallible copy.
  It covers payload chunks, directory key/value copies, merge edge copies and
  copied result rows. A lower-level copy and its later result-row assignment are
  distinct real copies.

The frozen native producer case has three base inserts, two later relationship
deletes and one merged survivor hidden by a tombstoned endpoint. Each of the
five physical entries is visited by decode, merge preflight and merge copy
walks, then the merged survivor is examined:

`(3 + 2) * 3 + 1 = 16 adjacency events`.

Its copied-byte total is:

`32 merged Edge + 292 relationship fields + 160 first endpoint fields + 160
second endpoint fields = 644 copied bytes`.

The result has zero rows, so this exact delta detects undercounted deletion and
prefilter/liveness work. A limit of 15 refuses the sixteenth merged examination
with the full-width `u128::MAX` output sentinel unchanged. A copied-byte limit
of 643 stops at 636 before the next eight-byte field, also with unchanged
output. A separate three-row relationship scan proves its copied-byte delta is
exactly the count-only path plus three `RelationshipRow` assignments; refusing
the third assignment leaves its sentinel unchanged and does not charge it.

## Literal RED to GREEN

The implementation began with named tests before the production interfaces
existed. The initial `graph_query_storage` build failed with E0599 for missing
`TreeResources::for_query` and with the missing typed runtime error branch. The
range test next failed to compile for missing `RangeScratch::for_query`.

Observed behavioral RED runs and their corrections were:

| Check | RED | GREEN |
| --- | --- | --- |
| cumulative lookup limit | `392b8239-d151-4768-a11b-dec00508cd38`: a second lookup was accepted | `e87c7e0e-7c04-425e-941b-e5e9587149f5`: typed `Lookups` refusal |
| payload copy limit | `3728e721-7b90-46e4-ba5b-0263b92c9cb0`: an extra copy was accepted | `a056c523-317d-4e64-b006-519cc7b87c08`: refusal before mutation |
| exact scratch charge | `5f8e42fa-b80d-4649-a7c1-414fbf6d00db`: 198,056 observed versus 197,992 expected | `ac5ef980-c218-4f8b-ad8a-f576aa74bfb2`: embedded arena descriptor removed |
| actual producer adjacency count | `0b791138-c32a-4fae-801a-a1b6a9d48a43`: 0 observed versus 16 expected | `b0bb4597-84a8-45b0-a627-7fcf10bce169`: actual codec and merged-edge sites connected |
| exact copied bytes | `d17a6030-dbcf-46ae-8485-27193401aaac`: 0 observed versus 644 expected | `afec93aa-7e2e-4646-aeb8-b2178346b8eb`: actual copy sites connected |
| copy threshold | `d7cbfa11-1295-4a9d-93aa-41fddde6f3fc`: the tightened 643-byte run exposed the 636-byte last accepted boundary | `00e29b9c-88ef-4597-9e46-4f57598681f6`: typed refusal before the next eight-byte copy |
| physical-entry can-fire | `7db0ceb4-30be-4f56-aab1-8cf690bd4541`: planting one skipped codec event produced 1 versus 16 | `45afe08d-da83-4df0-8c5a-2a5cee259860`: exact hook restored |
| same-memory scratch context, private guard | `91205901-f0c0-40b4-96e9-182ff21d3e58`: runtime B reached the source instead of returning the owner error | `6a92e7fe-6653-4838-902d-073fe023fe12`: owner rejection before source/work/backing changes |
| same-memory scratch context, frozen producer | `282f3b1b-73c7-489a-b177-ac171225988e`: real `validate_range` accepted runtime B | `e8a61d4c-fef1-4cd5-b85f-c6d8d4c97c44`: validate and both reader entry shapes reject B; runtime A remains usable |
| deadline at storage checkpoint | `f738d9ef-b417-424f-8604-66cca9afe62d`: removing the query `TreeResources::step` checkpoint allowed one source access after expiry | `2ca4aea5-b362-4081-baa4-817ee4d528e8`: typed timeout before source, copy, work, counters or charges |
| full-width private node leaf | `139f9df3-3375-4579-a79f-c478f2e07d4e`: truncating the lookup key to eight bytes failed the direct leaf on the checked 16-byte key width | `4b04e7a4-e082-4772-876b-66ba81c29f4f`: exact full-width lookup restored |

The intentional mutations were immediately restored. They are evidence that
the exact assertions can fail, rather than alternate product states.

The direct private-leaf test prepares genuine canonical images, provenance,
native records, a retained tombstone and a Nodes directory in one source. It
looks up `u128::MAX - 17`, preserves present-empty text, reads both original
vector coordinates (`1.25`, `-2.5`) after reborrowing query resources, proves
an absent text/vector stays absent, and distinguishes a tombstone from a
missing key. Its returned `NodeRecordState<'source, S>` is passed through an
explicit source-lifetime helper and its payload views remain usable only while
that immutable source is retained.

## Changed-path adversarial baseline

The pre-change baseline came from an isolated `git archive` extraction of the
exact prerequisite parent
`3c2fba32e72a03efdfa50f9ad7a51cdd2e8bcd72` at
`/tmp/ze135-parent-baseline-3c2fba3`. It ran the four pre-existing adjacency and
native-directory probe/runner tests with `-j4 --retries 0`: 4/4 passed, 467
skipped, run `886bbaf9-b704-4d21-a4fd-6a2bd6b0fc49`. The final source ran the
same four tests: 4/4 passed, 469 skipped, run
`538d49bc-df8f-4d58-a241-00112b9e19d2`. Exact commands and output are in
`raw/review-parent-adversarial-baseline.log` and
`raw/review-final-adversarial-matched.log`.

PG19 is deliberately separate because its additive query-storage module does
not exist at the prerequisite parent. On final source its probe and one real
runner episode passed 2/2 in run
`677147f2-bb7a-4b0f-a533-de477e5f5937`; this remains payload/workspace/control
evidence and is not represented as native adjacency fault coverage.

## Focused qualification

All commands ran from
`/Users/aghatage/Documents/code/zeppelin-embed-wt-ze-135` with stable Rust
1.93.0. The final logs and manifest in this directory record the post-review
terminal rerun.

The focused command set is:

```text
cargo nextest run -p zeppelin-embed --features graph-cypher \
  --test graph_query_storage -j4 --retries 0
cargo nextest run -p zeppelin-embed --features graph-cypher \
  --lib -E 'test(node_payload_leaf_preserves_full_identity_optional_payloads_and_state)' \
  -j4 --retries 0
cargo nextest run -p zeppelin-embed --features graph-cypher \
  --test graph_storage_prepare -j4 --retries 0
cargo nextest run -p zeppelin-embed-workspace-tests --features graph-cypher \
  --test adversarial_tests -j4 --retries 0 \
  -E 'test(query_storage_probe_reaches_actual_faults_and_same_seed_controls) | test(one_runner_episode_reaches_required_query_storage_contracts)'
cargo clippy -p zeppelin-embed --features graph-cypher \
  --test graph_query_storage --test graph_storage_prepare --no-deps -- \
  -D warnings
cargo clippy -p zeppelin-embed-workspace-tests --features graph-cypher \
  --test adversarial_tests --no-deps -- -D warnings
```

The terminal post-review runs were:

| Scope | Result | Log |
| --- | --- | --- |
| query storage | 7/7, run `fb9e6e9c-6869-4856-8e85-be5ec49dc86c` | `raw/final-query-storage.log` |
| private scratch owner guard | 1/1, run `d3c131b8-b73e-4e6c-bc62-c133088b4dfc` | `raw/final-scratch-owner.log` |
| storage preparation | 10/10, run `e64c69f4-117c-4358-a897-1fb8c66f5350` | `raw/final-storage-prepare.log` |
| PG19 probe plus real runner episode | 2/2, run `e15a44bc-f433-4ada-acd4-93628ec1ceac` | `raw/final-pg19.log` |
| scoped core Clippy | pass with `-D warnings` | `raw/final-clippy-core.log` |
| scoped PG19 Clippy | pass with `-D warnings` | `raw/final-clippy-pg19.log` |
| exact owned-file rustfmt | pass | `raw/final-format.log` |
| diff check and 45-file preservation | pass; 45 checked, 0 mismatches | `raw/final-diff-check.log`, `raw/final-preservation.log` |
| review deadline RED | intended failure; source calls 1 rather than 0, run `f738d9ef-b417-424f-8604-66cca9afe62d` | `raw/review-storage-deadline-red.log` |
| review deadline GREEN | 1/1, run `2ca4aea5-b362-4081-baa4-817ee4d528e8` | `raw/review-storage-deadline-green.log` |
| review private leaf RED | intended 8-byte-key failure, run `139f9df3-3375-4579-a79f-c478f2e07d4e` | `raw/review-node-leaf-red.log` |
| review private leaf GREEN | 1/1, run `4b04e7a4-e082-4772-876b-66ba81c29f4f` | `raw/review-node-leaf-green.log` |
| review terminal query storage | 8/8, run `8dd7ff0b-d9cc-4677-8c86-56c137aa051f` | `raw/review-final-query-storage.log` |
| review terminal private leaf | 1/1, run `7e6dfc57-b693-4c63-bc1a-b880f7215743` | `raw/review-final-node-leaf.log` |
| exact-parent adversarial baseline | 4/4, run `886bbaf9-b704-4d21-a4fd-6a2bd6b0fc49` | `raw/review-parent-adversarial-baseline.log` |
| matched final adversarial controls | 4/4, run `538d49bc-df8f-4d58-a241-00112b9e19d2` | `raw/review-final-adversarial-matched.log` |
| final PG19 probe plus runner | 2/2, run `677147f2-bb7a-4b0f-a533-de477e5f5937` | `raw/review-final-pg19.log` |
| final native 16/644 producer | 1/1, run `edb5069d-0fd2-46fe-b31c-385600265c02` | `raw/review-final-native-producer.log` |
| review scoped Clippy | core and PG19 pass with `-D warnings` | `raw/review-clippy-core.log`, `raw/review-clippy-pg19.log` |
| review format/diff/preservation | pass; 45 checked, 0 mismatches | `raw/review-format.log`, `raw/review-diff-check.log`, `raw/review-preservation.log` |

The actual-API lifetime harness used the source in `nonescape/negative.rs`.
Against the pre-correction frozen source it compiled successfully
(`raw/nonescape-before-green.log`). Against the corrected source it failed
only at the attempted `*slot = None` replacement with E0506 because the live
scratch retains the runtime borrow (`raw/nonescape-after-e0506.log`). The
`positive_nonescape.rs` control without replacement and
`positive_resource_reuse.rs` control that uses the same resources twice both
compile (`raw/nonescape-positive-green.log`). The temporary harness was outside
the repository, used an offline path dependency, and added no dependency.

PG19 registers nine additive
`property-graph.query-storage.*` keys. Four fixed seeds exercise the real query
workspace, scratch, lookup/scan, framed payload copy, copy-limit, cancellation,
memory-refusal, same-seed clean controls, comparator can-fire and reservation
release paths. One actual runner episode reaches all nine keys. PG19 does not
claim adjacency fault coverage; the frozen native producer test supplies the
separate 16/644 thresholds and physical-event mutation.

Exact formatting was applied only to owned Rust paths because an inherited
`storage/records.rs` import-order difference makes workspace-wide
`cargo fmt --all -- --check` unsuitable for this isolated delta. Direct
`rustfmt --edition 2024 --check` passes on all owned Rust files, and
`git diff --check` passes.

Broad workspace, campaign, coverage, fuzz and size runs were not performed;
they remain ZE-118. The component fixture is not evidence for public native
admission, protected lazy-open behavior, or final lifecycle qualification.

## Main integration inventory reconciliation

ZE-133 and ZE-138 landed before ZE-135 on main. The additive cherry-pick retained
PG18, PG19, and PG20, while the exact registry assertions still reflected the
pre-PG19 totals. Integrated run `29071748-ce08-4d7a-9b32-f44cd3ce4771` was
the intended RED: 186 keys were observed versus stale 177. The reconciled
exact totals are 186 graph-only and 198 with the 12-key PG16 hook. Root's
integration receipt records the subsequent focused GREEN runs; no key or
feature gate was removed.
