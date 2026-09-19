# ZE-73 versioned fixture and independent primitive oracle

Status: focused tooling GREEN; candidate awaiting independent review/integration.
Base main: `234dd4fe4ab45c55e16bf7866556e8b78f42ee8b`.
This evidence does not qualify the graph product or assert release completion.

The implementation adds graph-fixture-v1 to existing benchmark tooling, a
std-only primitive graph/revision/query oracle, and PG13 comparator probes in
the actual adversarial runner. No production engine code, external dependency,
Cargo.lock entry, source-reuse package or third-party code was added. The existing
seeded_rng helper moved into tests/tooling_seed.rs; three previously observed
named-stream word sequences remain exact. JSON stays in benchmark tooling;
expected-value code imports neither engine nor generator helpers.

The approved fixture recipe and engineering review are recorded in
[recipe-proposal.md](ze-73/recipe-proposal.md). CLI/adapter use is documented in
[graph-fixture-v1.md](../../crates/zeppelin-embed-bench/graph-fixture-v1.md).
The source references are qualification.md, retrieval.md and the pinned Kuzu
sparse/new-node/mixed-edit cases linked by those plans. No source was copied
from Kuzu. ZE-109's endpoint tombstone semantics supersede old degree-based
DETACH refusal wording.

## Observed inventory and serialized small fixture

The topology test enumerated every baseline and stress node and relationship,
including actual per-kind, in/out/total-degree and optional-population counts.
Raw histograms are in `ze-73/raw/final-tooling.log.gz`.

| Observed logical inventory | Baseline | 10x |
|---|---:|---:|
| Nodes | 266,375 | 2,663,750 |
| Relationships | 1,003,750 | 10,037,500 |
| Vector-bearing nodes | 182,500 | 1,825,000 |
| Derived raw f32 bytes, dimensions 768 | 560,640,000 | 5,606,400,000 |
| Project0 incident degree | 24,651 | 246,375 |
| Initial batches | 9,204 | 92,032 |
| Indexed text / both modalities | 250,900 / 177,025 | 2,509,000 / 1,770,250 |
| Vector-only / text-only / neither | 5,475 / 73,875 / 10,000 | 54,750 / 738,750 / 100,000 |

The actual serialized small fixture contains 131 nodes, 440 relationships and 80
vectors. It emits a 23-node shared preload and four 137-change meeting batches.
State B selects one node and eight relationships, emits 1/8/4-change batches,
and recreates four deleted relationship keys at revision 3 in a later batch.
It retains only the final four-change batch as its requested active tail.
The public file reader assertions check key continuity, changed names/text
length preservation, revision/deletion preconditions, logical mutation ordinals
and admitted-generation-relative expectations. The driver records maintenance
and post-barrier generations; fixture ordinals never prescribe them.
Each file has a SHA-256 and byte length; altered/missing files reject. Two fresh
same-seed generations are byte-identical. Literal complete byte-file digests for all six files, plus independent two-axis
coordinate bits, pin generator output.

Production adapters resolve namespaced keys to existing or same-batch references
and retain actual returned full IDs. Fixture indices never prescribe engine
IDs. Checkpoint/consolidation barriers and 64-envelope/16MiB thresholds are
requested actions, not observed engine writes/run counts. Full baseline/stress
payload files and exhaustive product comparisons were not run in this ticket.
They remain qualification work; the complete-scale topology enumeration above
is distinct evidence. Declared generator counter/permutation bytes are tooling
storage arithmetic, not measured allocator high-water or engine memory.

## RED and terminal GREEN

Named public seams were approved in the recipe before tests. Initial missing
fixture APIs and generator APIs failed compilation; these are API-existence RED,
not assertion evidence. Runtime RED covered missing graph edges, key resurrection,
all five initially unimplemented named-query forms, wrong empty-name rejection,
live Recreate's wrong error, and Restrict incorrectly treating two endpoint
removals as explicit relationship deletion. The pre-restart lexical numeric
literal was corrected from 1.1928807498924636 to 1.1921031975168894 after independent
arithmetic of its written formula; the incorrect literal is not product failure.

The original seven deliberate source mutants each exited 100 at the intended assertion:
accept missing edges; narrow u128 IDs; permit deleted-key resurrection;
deduplicate output bags; use 10 rather than 11 random low bits; renormalize hybrid
weights per node; skip input SHA validation. `ze-73/mutation-results.json` records
exact original/restored SHA-256, commands and assertion checks. Every source
was restored byte-for-byte before terminal GREEN. The actual runner initially
failed `one_runner_episode_reaches_required_fixture_comparators` because PG13
was not called; after routing it, seed 0 ran 59 operations with zero violations
and all eight PG13 fire/clean ledger entries. Four independent seeds also ran
all four paired comparator controls. These are wrong-observation controls,
not claims of native VFS, power-loss or graph-product fault injection.

Store-v1 normalization has an additional exact-bit RED/GREEN control. A bare
norm ceiling returned 0.5 where the directed-rounding enclosure requires
bits 0x3fe000000000000b. The corrected independently implemented formula is pinned
in the recipe. A live vector outside eligibility also changes the full-live
anchor, with fused bits 0x3fee38e38e38e38f. Python/math.nextafter calculations are
recorded in `ze-73/enclosure-hand-calculation.json`. Neither assertion uses
score tolerance. Ordinary reference score comparison takes explicit numeric
bounds; IDs, rank/order, multiplicity, nulls and modality membership remain
exact.

Terminal commands (four isolated nextest processes, one libtest thread, retries 0):

```sh
cargo nextest run -p zeppelin-embed-adversarial-oracle --test graph_fixture
cargo nextest run -p zeppelin-embed-workspace-tests --test graph_fixture \
  --test adversarial_tests \
  -E 'binary(graph_fixture) | test(fixture_comparators) | test(fixture_seed_stream)' \
  --success-output immediate-final
cargo clippy -p zeppelin-embed-adversarial-oracle --all-targets --no-deps -- -D warnings
cargo clippy -p zeppelin-embed-bench --lib --no-deps -- -D warnings
cargo clippy -p zeppelin-embed-workspace-tests --test graph_fixture \
  --test adversarial_tests --bin graph-fixture --no-deps -- -D warnings
```

The original checkpoint passed 17 oracle tests and seven tooling/runner tests.
Independent review then found the two gaps recorded below. The repaired
checkpoint passes 20 oracle and five generator tests (25 in one nextest run);
the three unchanged runner/seed checks retain their earlier evidence, giving
28 unique focused tests. The oracle has complete literal projections for all
six named read forms;
additional controls cover optional legs, full-live lexical stats, phrase order,
full-ID ties, prelimit bag multiplicity, empty eligibility, relationship-unique
self/parallel paths, replay/mixed generations, atomic rejection, deletion fences,
original float bits, typed/untyped empty lists and empty/NUL UTF-8 keys/names.
Strict targeted clippy and owned-file formatting/diff checks pass. Raw command
output and host/tool versions are retained under `ze-73/raw/` and `host.json`.
Host: Apple M3 Max, 16 logical CPUs, 128 GiB, macOS 27.0/26A5388g, arm64,
Rust 1.93.0, nextest 0.9.145. Test elapsed times are not performance gates.

## Independent review corrections

The bounded review of candidate `66f3e4b` requested two corrections; its exact
report is retained in `ze-73/independent-review-initial.md`.

Semantic context formerly expanded supplied seeds and distances. The new
`semantic_context_derives_nearest_full_live_seed_before_expansion` failed at
runtime: supplied ID 4/distance 0.25 expanded three rows while omitted live
ID 99/distance 0 was the correct nearest seed. `Query::SemanticContext` now
accepts vector/k and independently computes ordered-f64 distances over every
live original-f32 vector before sorting by distance/full u128 ID and applying k.
It then expands the unchanged full context projection. The top-20 test has
21 tied chunks with identical low64 bits in reverse input order and a parallel
MENTIONS edge: exactly 61 rows survive, including both parallel rows. An
unexpandable nearest vector still consumes k, and invalid vectors reject.
Two new source mutants (narrow tie IDs, omit k truncation) each exited100 at
the named assertion and were restored to identical source SHA-256; see
`ze-73/review-mutations.json` and matching compressed raw logs.

The generator formerly equated write count with graph generation despite
requested maintenance barriers. The new generation-trace test first failed
at runtime on the missing mutation-ordinal/relative-generation contract.
All A/B batches now record `mutation_ordinal` and an exact generation increment
of one relative to the admitted view. The manifest requires the driver to
record maintenance's returned generation and the post-barrier admitted state.
The focused simulated maintenance trace independently expects changed writes
at `[1,2,3,4,5,7,9,11]`. This is tooling metadata proof, not real maintenance.
Validation rejects incompatible generation contracts. Only A/B JSONL bytes
changed: their new complete SHA-256 values are
`66ff4862548c8449ceaa3f947baa284e1b93741171f75c0b803456e64e0f061a`
and `d09142a37ed05be7b9f5740d5bf1c8b7e3b79ba1c8960730d52ccfc22dcd4115`.
All three vector files and queries.jsonl retain their earlier literal digests.
The fresh small CLI manifest is `ze-73/small-manifest.json`.

Terminal repair command:

```sh
cargo nextest run -p zeppelin-embed-adversarial-oracle \
  -p zeppelin-embed-workspace-tests --test graph_fixture --test-threads 4
```

Observed: 25/25 GREEN, four isolated test processes, retries0, 4.325s elapsed
(supporting timing only). Scoped strict Clippy for oracle all-targets, benchmark
lib and workspace graph_fixture passed. CLI generate/validate passed. Repair
RED/GREEN, mutant and Clippy logs are retained under `ze-73/raw/review-*`.

## Boundaries and deferred qualification

This commits tooling, generator byte stability and primitive expected-value
behavior. It does not implement a GraphStore, publish/read a real graph view,
qualify recovery/durability, run full baseline/stress scoring, or qualify Rust/C/
Swift/installed consumers. The independent snapshot oracle runs outside timed
requests and may retain complete primitive fixtures; a benchmark process must
not keep that oracle beside its engine. Language/profile conformance, public
adapters, full-corpus truth execution and integrated performance stay with their
own dependent tickets. The separately required cypher-meeting-metadata seventh
timing cell is not substituted by these six read queries.

Per the owner's instruction, broad adversarial/workspace suites, per-crate 90%
coverage and release/footprint qualification are deferred through ZE-118/E12.
No deferred suite is claimed passed. The focused real-runner episode above is
solely the new comparator routing check, not a broad campaign result.
