# ZE-38 main integration

Candidate1185275afcdb958741530f8a258141a67a63fc4f was cherry-picked onto
main77695e0461ca8e7d37c68fff7fe83e80188b4923. Integration retained all prior
key-lifecycle/query modules, guides, probes and tests, appending the WAL ones.
No production WAL algorithm was changed during integration. Root reviewed the
framing, checked-watermark scan, mandatory descriptor/semantic hooks, provenance,
mutation, maintenance and low-level codec. The final-callback cancellation defect
was reproduced and fixed before the candidate; its exact final delta was reviewed.

The integration audit proves20 unaffected candidate source/test paths exact,
five previous guide/module/test files retained as full byte prefixes, all78
previous fixture files unchanged, all55 candidate evidence artifacts exact, and
all45 inherited user file hashes unchanged. The two shared runner/coverage files
combine existing and new probes/keys; their merged diff is retained here.

On this combined main source, focused nextest passed90 core/framing/legacy-WAL
regressions (three previously ignored tests), two zero-allocation audits,
three independent oracle tests, and two directed/actual-runner tests. The latter
covers seeds0,1,42,u64::MAX at the PG7 probe boundary and one actual seed0 runner
episode:59 operations,0 violations, all10 PG7 required keys reached. Scope and
fixture-validator limits remain exactly those in ../verification.md. No whole
GraphStore, publication, reclamation, platform or full-coverage claim follows.

A new actual-runner routing assertion received literal RED: removing only the
runner's graph_wal::probe call failed with the missing WAL-prefix coverage key.
Exact runner SHA256 was restored; the same test then passed. No mutation remains.
Strict Clippy for core, oracle and workspace-test packages, all targets with
allocation-audit/test-support, and workspace format check passed. The identical
restored source received the targeted final GREEN; no broader rerun was needed.

commands.json contains exact commands/exit codes; compressed logs preserve output.
runner-control.json records the deliberate RED/GREEN and restored source hash.
Broad workspace/adversarial/coverage/release qualification remains ZE-118/E12.
