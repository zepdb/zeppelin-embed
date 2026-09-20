# ZE-133 native adjacency oracle runner evidence

## Scope and source identity

This evidence binds the ZE-129 primitive model to the actual private native
participant. The adapter stages real `StagedBatch` values, calls
`prepare_native_graph`, finishes every `PreparedObjects` pack, writes and syncs
the object files, reopens them under a controlled outer-frame admission, and
uses the real directory/range readers and `NativeGraphReader`. Expected state
comes only from primitive operations. Observed rows are never sorted, repaired,
deduplicated, or derived back into the model.

- Branch: `codex/ze-133-adjacency-runner`
- Exact prerequisite and checkpoint parent:
  `db98cc516cdf90194003ad8d7ece102eedad68dd`
- First immutable adapter checkpoint:
  `c7aeeb5a7ddcb2ee3159750993cfdc6213a79875`
- Producer `prepare.rs` restored SHA-256:
  `464fd42dcf65d229d4ecabc5222d180cee5d3b897806f01c70981ca1584f8e88`
- Final adapter SHA-256:
  `e36cd9399bf94921d498785cb78b2068c64d974d8eb3e76e386a29ef1817ff6e`
- Final registration SHA-256 values:
  - `tests/adversarial/mod.rs`:
    `b8f14dc5547b58b584374f62f6343d4e28ba01852cb29595a010817cba5c8551`
  - `tests/adversarial/runner.rs`:
    `92ae5f676548445f0b49fb35f917020940fb6f2f82fde1608e5c74485059dfbf`
  - `tests/adversarial/coverage.rs`:
    `fb12dae873e61933324d3c5b3442c6ca69b33624607321638a2321917777272b`
  - `tests/adversarial_tests.rs`:
    `223d3d1e3c51b1334445711c65e64690f4c4a5a73b84742c23a7646c139659a6`

The machine was an Apple M3 Max running macOS 27.0 build 26A5388g,
Darwin 27.0.0 arm64. The toolchain was rustc 1.93.0 and cargo 1.93.0.

## Named RED and actual producer controls

The initial named test was run before the adapter implementation:

```text
cargo nextest run -p zeppelin-embed-workspace-tests \
  --features graph-cypher --test adversarial_tests -j 4 \
  -E 'test(native_adjacency_store_probe_reopens_actual_emitted_participant)' \
  --success-output immediate
```

Nextest run `5fc1ade4-83b9-4f54-af0b-dec7dabcfb0b` was RED, 0/1, with
`PG18 native adjacency adapter is not implemented`.

Two temporary source mutations were then applied to the real producer while
the final adapter SHA above stayed unchanged. Neither mutation was committed.

1. The missing-reverse mutation removed the IN tuple from the producer's
   `collect_changes` loop. Mutant SHA-256 was
   `ea3b91201ba8948a7a117e9ec13d461b402c004eb722586849b7022889902615`.
   `prepare_native_graph` returned the candidate; the adapter finished,
   synced, reopened, and observed it. Nextest run
   `e4db854f-313d-4ec9-8b67-17d669096c30` was RED, 0/1, at the independent
   comparison: `raw_incoming.length`, expected 5, observed 0.
2. The ignored-delete mutation made the returned candidate retain the prior
   authoritative relationship, OUT, and IN roots for a relationship deletion.
   Mutant SHA-256 was
   `95e0f6418cdc2f43444901a4c20aebeaa7c7118c338b1416c4398d62a331d1c4`.
   The candidate again completed the same finish/sync/reopen path. Nextest run
   `735c8d4a-975e-4285-8397-435746a8326b` was RED, 0/1, at
   `raw_relationships.length`, expected 4, observed 5.

Two earlier ignored-delete experiments did not count as producer-oracle
controls. Replacing the deletion action with `continue` used source hash
`25033ea0e8fa2c989a59bcc66b7f8b7cac7ece8b8e51ac17e031327c1b1d1d9d`
and run `277ea2df-d93b-46da-9552-ad4ae862c078`; retaining only OUT and IN
used source hash
`53f5e5a228ca1fe72e6157ebdb2024ccbe0c80feaf2eb6b502d6c75c0c63c8a9`
and run `190f506a-1d5c-4363-8c1a-48f5a34280e0`. Both stopped with the typed
`missing graph directory artifact` error before an independent observation
could be compared. They are guard detections only.

After each experiment, this command returned the original hash and exit 0:

```text
shasum -a 256 \
  crates/zeppelin-embed/src/property_graph/storage/adjacency/prepare.rs
git diff --exit-code -- \
  crates/zeppelin-embed/src/property_graph/storage/adjacency/prepare.rs
```

## Terminal GREEN and runtime receipts

After the final restoration, this focused command was GREEN:

```text
cargo nextest run -p zeppelin-embed-workspace-tests \
  --features graph-cypher --test adversarial_tests -j 4 \
  -E 'test(native_adjacency_store_probe_reopens_actual_emitted_participant) \
      | test(one_runner_episode_reaches_native_adjacency_store_contracts)' \
  --success-output immediate
```

Nextest run `791f178d-1330-4417-9d61-a81c3bf539cf` passed 2/2 with 471
skipped. Seeds 0, 133, and `u64::MAX` each reported:

- 5 exact independent comparisons and 2 separately labeled participant-root
  selection comparator controls;
- 4 finalized/reopened object files and 20 present-root receipts across the
  four successful generations;
- 2 typed fault fires paired with 2 same-seed normal controls;
- normal append count 19, injected failure at append 9, one abort object, and
  zero root keys from failed candidates;
- normal work 1,179,533, failing limit 1,179,532, and 1,179,485 work charged
  before the typed work refusal;
- one typed plain-DELETE refusal and 10 exact cleanup checks;
- storage reservation 80 -> 80 bytes;
- retained fixture/file backing 494,606 bytes, followed by shared reservation
  152 -> 152 bytes after the owners were dropped.

The primitive cases include full-width IDs, same-batch endpoint creation,
self-loops, parallel relationships, sparse nodes and relationship types,
property-only edits, DETACH with raw retention, explicit raw relationship
deletion, plain DELETE refusal, and rereading retained old roots. Raw
relationship, OUT, and IN rows are observed separately from endpoint-liveness
filtered relationship scans, expansion, count, degree, and limited ranges.

## Registration and focused gates

PG18 adds these 19 `graph-cypher` coverage keys:

```text
property-graph.adjacency-store.native-history
property-graph.adjacency-store.full-id
property-graph.adjacency-store.same-batch
property-graph.adjacency-store.self-parallel-sparse
property-graph.adjacency-store.property-only
property-graph.adjacency-store.detach-raw
property-graph.adjacency-store.raw-delete
property-graph.adjacency-store.plain-delete-refusal
property-graph.adjacency-store.old-root
property-graph.adjacency-store.reopen
property-graph.adjacency-store.emitted-keys
property-graph.adjacency-store.participant-selection.missing-reverse
property-graph.adjacency-store.participant-selection.ignored-delete
property-graph.adjacency-store.append.fire
property-graph.adjacency-store.append.clean
property-graph.adjacency-store.budget.fire
property-graph.adjacency-store.budget.clean
property-graph.adjacency-store.no-partial-candidate
property-graph.adjacency-store.private-release
```

The legacy default registry remains 88 keys. The prior native registry had 159
source keys: 147 ordinary `graph-cypher` keys plus 12 PG16 keys gated by
`graph-result-test-support`. PG18 preserves all of them and produces 166 active
graph-only keys, or 178 when the separate PG16 hook is selected.

- Default registry gate run `87b3d38b-8417-41da-914b-b87533e155db` passed
  1/1 and confirmed no property-graph keys without `graph-cypher`.
- Graph-only registry gate run `b46d88c3-b8e4-4b33-8e46-4b6210f7de7a`
  passed 2/2 and confirmed all 166 graph keys active while all PG16 response
  keys remained absent.
- Separate PG16-hook registry run `6e9cb338-34d6-4cc7-b6be-a585a2ba933b`
  passed 2/2 and confirmed the 178-key native total and all 12 response keys.
- The first graph-count assertion expected 178 graph-only keys and failed in
  run `d9a3e829-4575-4e6f-8d03-a97837826d98`. The 12-key difference was the
  intentional PG16 hook boundary, so the assertion was corrected to 166; no
  registry or feature gate was weakened.
- Strict scoped lint passed:
  `cargo clippy -p zeppelin-embed-workspace-tests --features graph-cypher
  --test adversarial_tests -- -D warnings`.
- Scoped `rustfmt --check` on the four owned Rust registration/test files and
  `git diff --check` both exited 0. A workspace-wide formatting check was not
  used as acceptance because it reports pre-existing ordering in the pinned
  producer `storage/records.rs`; that file remains byte-identical.

The 45 inherited paths in `/tmp/ze-133-preservation.json` all passed SHA-256
verification. The checkpoint manifest itself remained
`6c94511164a3072908689f6e54c8ef8eec738d5948b7013996e44e6bd999a284`,
and all 26 entries in that manifest passed, including the 12 production files,
the five native tests/evidence files, and the checkpoint-only logs and scope.

## Qualification boundary

This proves the private native adjacency candidate, actual emitted packs,
reopened file observations, independent primitive oracle, seeded runner
registration, paired preparation faults, and private resource release. It does
not claim a ZE-45 public read lease, atomic publication, WAL recovery, public
traversal, or broad workspace/adversarial/coverage/size qualification. Those
broad obligations remain with ZE-118.
