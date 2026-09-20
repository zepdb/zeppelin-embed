# ZE-153: native relational execution after ZE-152

Source: main `f08898e6d34c3983ab07dc085567481e2ae81c18`, inspected in `/Users/aghatage/Documents/code/zeppelin-embed` on 2026-09-20. ZE-153 is root's claimed planning ticket. Source inspection only: no builds/tests, product/tracker changes or further agents.

## Final recommendation

**Ready for one complete private native relational/eligibility implementation component that owns the substantive extension of NativePattern's existing occurrence engine.** Root explicitly approved that ownership and the representative rule below. The exact finite plan is `/tmp/ze-153-native-relational-execution-plan.md`.

Implement Aggregate, Distinct, Sort and OffsetLimit inside the existing occurrence tree; reuse ZE-125's actual Rows kernels, ZE-145's evaluator, ZE-149's typed driver and ZE-152's native traversals/joins/optional anchors. Add native singleton eligibility preparation using the existing EligibleNodeSet. Execute actual MATCH → WITH aggregate/DISTINCT/ORDER/LIMIT → MATCH and legal join/optional compositions. This is one production component, without a terminal-suffix restriction, second engine, compiler pass, public facade or completed-result packager.

Ordinary wrappers alone were insufficient because NativePattern recursively owns its subtree and rejects the four relational operators. The final ownership decision resolves that restriction within the same occurrence owner; no separate speculative interface ticket is proposed.

## Existing source and exact additions

Paths below are relative to `crates/zeppelin-embed/src/property_graph/`, except explicitly named Cypher paths.

| Existing source | Consequence |
| --- | --- |
| `query/pattern.rs:274`, `1913`, `1921` | Authentic view/RuntimePlan construction, native RowOperator and typed PullOperator already exist. Extend this owner. |
| `query/pattern.rs:1958–1988`, `2019` | Count/build must recognize the four added operators while preserving lexical anchors. |
| `query/pattern.rs:66`, `232`, `599`, `1882` | RelationshipUse, per-row uses, reset and uniqueness stay inside the existing owner. |
| `query/expression.rs:519`, `574`, `640` | Reuse scalar evaluation; copy scratch before subsequent evaluation. Aggregate descriptors supply scalar operands; Aggregate itself is not evaluated as a scalar. |
| `query/relational.rs:81`, `158`, `224` | Reuse charged storage/schema/map semantics; add one checked crate-private accessor for the selected original row. |
| `query/relational/ordering.rs:128`, `162` | Existing DISTINCT/sort remain the semantic implementation. Their order vector identifies physical representatives. |
| `query/relational/aggregate.rs:47`, `225–239` | Output key values already come from Group.first. Return precisely that same ordinal in a charged sidecar from this same kernel. |
| `query/eligibility.rs:26`, `111` | Existing packed-ID builder and exact-view access consume the newly executable real native singleton domain. |
| `query/runtime/driver.rs:308` | Existing typed execute_in remains the single final row drain/completion boundary. |
| `query/plan/mod.rs:640`, `740`; `query/plan/search.rs:27–77` | Use validated singleton/schema facts and actual Search input/request descriptors. |
| `crates/zeppelin-embed-cypher/src/lowering/projection.rs:132–186`; `lowering/mod.rs:69` | ZE-126 already emits complete read shapes with authentic owners; no compiler work is assigned. |
| `query/completed.rs:135`, `198` | ZE-127 owns copied storage; real entity result conversion/public packaging remains ZE-53. |

## Approved provenance rule

Root accepted this exact physical-row ownership rule:

- Sort, OffsetLimit and ordinary projection transport the selected input row's existing sidecar.
- DISTINCT transports its actual selected physical representative's sidecar, even when an equal visible row has different hidden uses.
- Aggregate transports exactly **Group.first's sidecar**, matching the row from which the existing kernel copies its key values. Empty global aggregation has no source use. Count/collect values invent no traversal origin.
- Do not union group uses, clear uses merely at With, or add a generic provenance framework/membership allocation.

The compiler allocates a fresh PatternId per MATCH (`lowering/pattern.rs:123–134`), so a legal later MATCH can reuse prior relationships: current uniqueness compares PatternId before relationship/origin identity. Structured With does not erase lineage: `query/plan/lineage.rs:50–61` follows With and aggregate-key aliases, and `crates/zeppelin-embed/tests/graph_query_plan.rs:1828–1912` verifies origins remain checked through With. Keep validation unchanged and retain the chosen row's sidecar until its real owner drops. Tests distinguish representative transport from both clearing and union, choose representatives through explicit order, and verify fresh-MATCH bags.

## Scope and decision mapping

Propose one flat ticket: **Implement native relational barriers and eligibility domains**. Genuine inputs are ZE-152, ZE-125, ZE-145, ZE-149, ZE-49, ZE-45/39 and ZE-123/126/138 typed contracts, plus this approved ZE-153 plan. Root records exact dependencies and specification/index changes. No dependency is removed; the planner claimed no implementation.

ZE-46 continues on disjoint storage/lifecycle/reclamation files. Root applies additive runner registration after the executor commit independently of ZE-46 completion, resolving later additive conflicts while preserving both. Actual registered-runner compilation is required before closure.

ZE-50/51/53/56 retain every original native/public/oracle/compaction/reopen/result/conformance obligation. ZE-64 retains ranking and eager CALL/report integration; eligibility preparation does not execute a search. ZE-118 retains broad/full/adversarial/performance/coverage/fuzz/release qualification.

## Decisive inspected SHA-256 values

```text
dd594ce2a71479873f918842aa042aeb8fb954b5b29e823cb8dfcfa8560b4525a  query/pattern.rs
1e0c00fe7cff7387de08b0adc62c88ee298bba5df94ed21c68856da5db3b5d68  query/relational.rs
738d1eb073f8199dccb2e49a7a5756cebfdb2b65e3fdb9785638c1dc92e2661c  query/relational/blocking.rs
978e45e22e4a4cef11854779f95f5a92e55d67ae956ce21862a659156a1bcb1f  query/relational/ordering.rs
2560c9d11bd7f08f13b1c7ec9fe852f32659dcf947c23effe561e463a9935697  query/relational/aggregate.rs
4e1aae72707ab86fa1c96eb8440f33d9c5765e6e70bfb2f56693170912af3e7b  query/eligibility.rs
6b64fd9fcd7e06b9a6c415784c47c2d3fb5cf974c2c42339f1cb9a921d10ff47  query/expression.rs
8616d16d5096a364d555ead966168d070a06ac73ee49bcfb027485678b92a1be  query/runtime/driver.rs
6c30deebbe4807a9b262ff31ce96fc16863d9a855209f2c881c7e49317b36a14  query/plan/lineage.rs
b232d3d4a839007c3540e9b8e1b0efd888dc09dfce823075c53b66060bc7d05d  crates/zeppelin-embed-cypher/src/lowering/projection.rs
```
