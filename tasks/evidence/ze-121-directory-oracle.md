# ZE-121: independent directory and key-fence model

Status: implementation candidate from main
`8517c9e5f2f75b6d35ca1cffbf96c5bcbed54978`; integration and ZE-43's production
adapter remain outstanding. This is a std-only oracle with no new dependencies,
engine imports, lifecycle-helper calls or physical/canonical codec reuse.

The model consumes independently authored primitive operations and checks
logical observations from a retained root. It preserves all 128 identity bits,
separate node/relationship domains, exact namespace/key bytes, canonical bytes,
complete provenance and persistent deleted-key fences. A snapshot owns its old
contents. Relocation changes only the view generation, and object inventory has
no path into entity liveness. Limits bound model operations, identities, fences,
key/image lengths, label counts and query capacities; these tooling limits do
not claim engine allocation accounting.

Under ZE-109, DETACH changes only its node: raw incident relationship/type rows
remain, observable relationship queries check both endpoints before applying
capacity, and full node deletion provenance remains until all stored incidents
are removed. Dropping a node tombstone never makes that identity reusable.

The [interface contract](ze-121/interface-proposal.md) was reviewed with the
ZE-43 owner before implementation. Numeric NamespaceId physical-key ordering is
separately owned by that adapter; the model uses logical exact namespace bytes.
This oracle does not sort, deduplicate or fill in production observations.

## Focused proof

Host: Apple M3 Max, arm64, macOS 27.0 build 26A5388g; Rust 1.93.0;
cargo-nextest 0.9.145. Exact host commands/results are in
[host.json](ze-121/host.json). Dataset: the 15 deterministic handwritten tests in
`tests/adversarial-oracle/tests/graph_directory.rs`; no randomized fixtures or
production-derived expected encodings. Literal fixtures include colliding low
64 bits, IDs 255/256/65536/u128::MAX, empty/NUL and distinct Unicode byte keys,
signed-zero and distinct NaN payload bytes, separate kinds with the same ID/key,
complete deleted/unkeyed provenance, old-root views and raw/visible membership.

| Evidence | Observed result |
| --- | --- |
| Missing module/API test for full IDs | Compile RED, E0432, exit 101 |
| Exact create replay in fence-history test | Runtime RED, `reused_identity`, exit 100; corrected replay GREEN |
| Relationship missing-endpoint test | Runtime RED, invalid relationship was accepted, exit 100; corrected GREEN |
| Missing range/maintenance and exact comparator methods | Compile RED, E0599, exit 101; implemented GREEN |
| NoOp at maximum revision | Runtime RED, `revision_overflow` instead of NoOp, exit 100; corrected before-increment classification GREEN |
| Terminal focused nextest | **15 passed, 0 failed, 0 skipped**, exit 0 |
| Strict oracle all-targets clippy | Exit 0 with `-D warnings` |
| Dependency inspection | Oracle package only, zero normal/build/dev dependencies |
| Formatting and diff checks | Exit 0 |

Exact terminal commands, executed from this ticket's isolated worktree:

```sh
cargo nextest run -p zeppelin-embed-adversarial-oracle --test graph_directory
cargo clippy -p zeppelin-embed-adversarial-oracle --all-targets --no-deps -- -D warnings
cargo tree -p zeppelin-embed-adversarial-oracle --edges normal,build,dev
rustfmt --edition 2024 --check tests/adversarial-oracle/src/graph_directory.rs tests/adversarial-oracle/src/graph_directory/compare.rs tests/adversarial-oracle/tests/graph_directory.rs
git diff --check
```

Nextest uses the committed default profile: four isolated test processes,
retries zero, one libtest thread per process. The terminal run ID is
`46aee53e-08c6-47a0-91e1-ed6ac1852416`; its summary reports 0.037 seconds of test
execution. That is a raw validation observation, not a performance claim.

[terminal-nextest-final.log.gz](ze-121/terminal-nextest-final.log.gz) and its
[command record](ze-121/terminal-nextest-final.json) retain the full result.
[Strict lint output](ze-121/strict-clippy-final-restored.log.gz) is separate.
The earlier `green-exact-comparator` attempt was **not** green: one test expected
the revision field before the earlier operation-kind disagreement. Its literal
first-difference assertion was corrected; no comparison was weakened. Initial
formatting/clippy fixes are likewise not behavioral RED evidence.

## Comparator controls

The positive complete Observation is handwritten independently of Model output.
Negative observations separately change high identity bits, dead fences,
omitted/extra/reordered labels and types, canonical bytes, every provenance
field, unkeyed tombstone provenance and old-root contents. They must report exact
first-difference paths; a candidate phantom node inferred from object inventory
must fail the complete directory count.

[Eleven source mutants](ze-121/mutation-results.json) each produced **exit 100
at its named runtime assertion**; there were no accepted compile-failure
substitutes. They narrow compared IDs, ignore dead fences, ignore labels or
types, ignore canonical bits or original generation, substitute observed state
for the old-root expectation, ignore unkeyed tombstones, disable image limits,
reverse logical key tuple ordering and allow retired identity reuse. The
[mutation script](ze-121/mutations.py) records exact mutations and verifies
pre-mutation/restored SHA256 equality after each case. The later NoOp ordering
correction has its own literal runtime RED and the final 15-test GREEN; it does
not change any of those comparator or limit control paths.

[Source hashes](ze-121/source-hashes.json) pin the reviewed implementation/test
files. [Artifact hashes](ze-121/artifact-hashes.json) pin the retained raw logs,
command metadata and scripts. All raw logs are gzip-compressed with mtime zero.

## Remaining qualification

ZE-43 still owns the actual production source/sink adapter, PG8 runner binding,
scheduled faults, production tree ordering, cancellation, reopen and immutable
artifact proof. Primitive model/control tests prove none of those by themselves.
No engine code, storage dependency or public product contract changed here.
Nonessential broad workspace/adversarial/per-crate coverage qualification stays
in ZE-118. No broad suite was run or reported as passing for this ticket.
