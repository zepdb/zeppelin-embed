# Native graph fixture tooling

`graph-fixture-v1` produces immutable, digested inputs for the approved graph
qualification. It does not ingest a Store, run a benchmark, or qualify a release.
The exact recipe is in `tasks/evidence/ze-73/recipe-proposal.md`; the authoritative
counts, query projections and scope are in `docs/graph/plans/qualification.md`.

From the workspace root:

```sh
cargo run -p zeppelin-embed-workspace-tests --bin graph-fixture -- \
  generate small /tmp/graph-fixture-small SOURCE_PIN
cargo run -p zeppelin-embed-workspace-tests --bin graph-fixture -- \
  validate /tmp/graph-fixture-small
```

Replace `small` with `baseline` or `stress` for the approved scales. `SOURCE_PIN`
is the exact reviewed checkout/build identity supplied by the caller; the
manifest records it verbatim together with compiler/host/PRNG/analyzer details.
The generator requires `shasum -a 256` and refuses to replace any existing output
file. A failed generation may leave an incomplete directory; validation refuses
it. Use a fresh output directory for another generation.

The six input files are `batches-a.jsonl`, `vectors-a.f32le`, `batches-b.jsonl`,
`vectors-b.f32le`, `queries.jsonl`, and `query-vectors.f32le`. `manifest.json`
records exact lengths and SHA-256 values. Vector offsets address little-endian
IEEE f32 bits; there is no implicit normalization when consuming those files.
JSON is benchmark tooling data, never a persisted engine representation.

Initial shared batches contain at most 256 nodes. Each meeting is one atomic
137-change batch containing 27 nodes and 110 relationships. Its endpoints are
namespaced keys: the product adapter resolves existing keys and uses batch-local
references for new nodes, then records the returned full IDs. Decimal fixture
indices are not engine IDs. Every relationship is keyed. Batches record expected
changed dispositions and logical mutation ordinals. Each changed batch expects
the actually admitted generation plus one; mutation ordinals exclude maintenance
and never stand in for physical graph generations. The driver records every
maintenance result and the post-barrier admitted generation.

State A requests a checkpoint/consolidation after initial ingestion. State B
updates floor(N/100) nodes and selects floor(E/50) relationships. Even selected
relationship positions update; odd positions delete at revision 2 and recreate
at revision 3 in later batches with expected deletion revision 2. Batches have at
most 128 changes and no duplicate key. Barriers after every nonfinal B batch
leave only the final batch as the intended active tail. These are requested
logical actions, not observed WAL bytes, physical run counts or durability
results. The product qualification driver must record those observations.

The generator streams one batch and one vector at a time. Degree counters,
selection permutations and selected correction metadata are separate tooling
allocations; their declared raw storage is reported in the manifest. No complete
vector matrix or graph oracle is retained in the generator. These declarations
are not an allocator high-water measurement or an engine memory claim.

The std-only `zeppelin-embed-adversarial-oracle::graph_fixture` module consumes
complete primitive snapshots and maintains keyed revision/incarnation histories.
Semantic context accepts a query vector and k, derives full-live vector seeds
with full-NodeId ties, then expands their context without refilling unexpandable
seeds or deduplicating parallel evidence rows.
Its six typed query forms calculate logical rows, eligibility, full-live BM25
statistics, scalar ordered-f64 squared-L2 truth, Store-v1 full-live original-f32
norm enclosures, optional hybrid modalities and relationship-unique paths.
Expected-value code imports neither the engine nor this generator. A later
public-surface adapter supplies observed opaque IDs and primitive payloads; the
oracle must run outside timed product requests. It intentionally rejects text
outside the pinned lowercase ASCII fixture vocabulary's analyzer subset.

Score comparison tolerances are explicit caller arguments. They cover only the
declared numeric evaluation difference between reference f64 and product f32;
full IDs, ranked order, multiplicity, nulls and modality membership stay exact.
Anchor policy has separate exact-bit controls and is not replaced by tolerance.
General engine/Cypher/C/Swift parity, offline full-corpus truth runs and timed
qualification remain their owning tickets and ZE-118's deferred broad suites.
