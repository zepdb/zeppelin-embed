# ZE-73 independent review

Verdict: changes requested for two specification gaps. This review does not qualify the graph product.

Reviewed candidate: `66f3e4b17d73d4b4da7958061a950f93ae9fe314`, parent `234dd4fe4ab45c55e16bf7866556e8b78f42ee8b`.
Reviewed source: frozen `/tmp/ze-73-review-2`, with final candidate test/evidence changes inspected from the candidate worktree. All 22 entries in the candidate's `tasks/evidence/ze-73/candidate-source.json` were independently compared with the committed blobs and matched. The frozen snapshot differs from that final manifest only in the subsequently expanded six-file digest test and evidence summary.

Critical SHA-256 pins:

- `tests/adversarial-oracle/src/graph_fixture/query.rs`: `52ba5c73b5f4fc1f6ac0dbc9152780a777ac852b0c313a0b523af10c29619496`
- `tests/adversarial-oracle/src/graph_fixture/model.rs`: `8e293961a5b83a2c4e031131726a8a9b8b35b60840a0abc6878246f48117ab8c`
- `crates/zeppelin-embed-bench/src/graph_fixture/files.rs`: `c61bfb774c261c62d7465b788563028380532697828098cde8dcbfc14f88783a`
- `crates/zeppelin-embed-bench/src/graph_fixture/topology.rs`: `c9f0b28f422a0c253cadb083c6cb41f191b15328540318b3b0dc22169330ffd2`
- `crates/zeppelin-embed-bench/src/graph_fixture/vectors.rs`: `c1fefc9b994f419440dcd17806626fa849b2751b6ffa56d16b901a9b62af2ceb`

## Standards review

No standards blocker found within the bounded review. The primitive oracle uses std only and imports neither engine nor generator code. No new dependency or production engine change appears. Expected-value arithmetic remains separate from the generator, preserves original scalar bits and full u128 IDs, and comparisons preserve duplicate observations/rows. Runner additions are explicit comparator discrepancy controls; evidence correctly distinguishes them from native fault/recovery qualification. Broad-suite deferral is recorded through ZE-118.

## Specification review

1. **P1 — semantic-context lacks independent vector seed truth.** `query.rs:20-22,110-153` accepts `seeds: Vec<(u128,f64)>`, validates only finite distance and existence, then expands those supplied seeds. There is no query-vector/k input or independent full-live ranking for this named query. Qualification lines 63 and 73 require top-k before graph expansion and independent complete query truth. Passing erroneous engine seed IDs/scores to this oracle can therefore bless that same wrong result. Minimal regression: meeting 1 links chunks 2 and 3; both mention entity 4; stored original f32 vectors are `[0]` and `[10]`. For query `[0]`, k=1, assert literal node-2/score-0 context output and reject node-3/score-0. Change the query interface to vector/k; independently rank all live vector-bearing nodes by ordered-f64 squared L2 and full-u128 tie order before expansion. An unexpandable nearest node must still consume k. Preserve parallel-edge output bags.

2. **P2 — absolute generation expectations ignore maintenance.** `files.rs:327` emits `initial_batches + batch_index + 1`, although lines 329 and 470 request checkpoint/consolidation between stages. `writes.md:102,112` permits maintenance to advance generation. A correct small-fixture execution can make A's five writes generations 1–5, consolidation generation 6, then first B generation 7, while the fixture demands 6. Record logical mutation ordinals separately and derive mutation expectations from the actually admitted generation, including barrier outcomes. Minimal regression: a trace adapter that advances generation at a requested maintenance barrier must accept the next write at current+1; do not suppress that maintenance advance to match the fixture.

## Verification and scope

Standalone narrow reproduction of finding 1 passed (exit 0): `rustc --edition=2024 /tmp/ze-73-independent-repro/semantic-context.rs -o /tmp/ze-73-independent-repro/semantic-context`, then that executable. It asserted both the independently known correct row differs and the supplied incorrect observation is exactly accepted. Source copies are byte-identical, recorded in `/tmp/ze-73-independent-repro/copied-source-hashes.json`; output is in `output.txt`. An initial harness-only path-module compile error was corrected by preserving Rust's normal module layout; no candidate source was changed. Finding 2 is a source/contract inconsistency with the exact arithmetic trace above, not a claimed product execution.

Reviewed batching 256 shared/137 meeting/128 corrections, topology/count/skew formulas, vector recipe, explicit six-file SHA/byte manifests, full-ID/self/parallel behavior, endpoint-tombstone filtering, key lifecycle, and policy-v1 full-live original-f32 norm enclosure. The enclosure formula and literal directed-rounding bit tests match the accepted recipe; full-live out-of-eligibility vectors affect the normalization anchor. No additional concrete blocker found in that bounded scope. Existing focused test/mutation results were inspected as evidence, not rerun or relabeled as independent full-workload acceptance. No broad suite, full fixture materialization, product adapter or qualification run was performed.

Both findings and the reproduction were sent to root and the ZE-73 owner before handoff. Source, tracker, existing worktrees and inherited changes were left untouched.
