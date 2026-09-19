# ZE-73 graph-fixture-v1 recipe proposal

Base: 234dd4fe4ab45c55e16bf7866556e8b78f42ee8b. Tooling only. The following
fills unspecified deterministic mechanics in qualification.md; it does not
change approved counts, limits, product scope or qualification scheduling.

## Preserved contracts

Scales 1/10 retain 9,125s meetings, 20 chunks and six alternating Decision/Action
items per meeting, and 10,000s Person / 2,000s Project / 8,000s Topic nodes.
All six edge families and multiplicities, project/participant hub formulas,
17,280-second timestamp spacing, fixed payload lengths/optional text residues,
every-seventh Shared label, every-tenth aliases, every-third relationship
properties, root seed 0x4752415048303031, 64 centroids, 768D, low-11-bit integer
coordinates, ordered scalar f64 normalization, 7/8 and 1/8 corpus mixture,
31/32 and 1/32 query mixture and final single f32 cast stay verbatim.
State B selects floor(N/100) nodes and floor(E/50) relationships, with even
relationship selection positions updating and odd positions delete/recreating.
Every generated edge is keyed. State-B batches have at most128 logical changes. Initial shared preload uses
256-node batches, and each initial meeting stays one137-change atomic batch
(27 nodes plus110 relationships), exactly as qualification.md specifies.
ZE-109 node tombstones supersede all older degree-based DETACH refusal/drain
text. This ticket implements expected logical fixtures, not product campaigns.

## Explicit engineering choices for review

- Epoch timestamp: 1,700,000,000 Unix seconds. Meeting m is that plus 17,280*m.
  Node ordinals are tooling keys only: shared Person, Project, Topic pools first,
  then each meeting followed by its 20 chunks and six items. Actual engine IDs
  are later adapter observations. Every namespace is `fixture-v1/<kind>` and
  decimal indices are unpadded. Relationships use `fixture-v1/relationship` with
  decimal meeting-local global ordinal m*110+edge_offset.
- Edge order: 20 HAS_CHUNK; each chunk's person/project/topic MENTIONS (60);
  six HAS_ITEM; six SUPPORTED_BY; each item's person/project ABOUT (12);
  five PARTICIPATED_IN; one FOR_PROJECT. Item i uses participant i mod 5 and
  supporting chunk (m+3*i) mod 20. Chunk j uses participant j mod 5.
  Participant collision advancement wraps among nonzero person IDs.
- Named ChaCha8 streams use the existing adversarial test_support::seeded_rng
  derivation verbatim (rand 0.9.5 / rand_chacha 0.9.0). Names are prefixed
  `graph-fixture-v1/`: topics, centroids, chunk-noise, query-noise, state-b-nodes,
  state-b-relationships, state-b-vector-noise. Each stream has its own seed;
  u32 words are consumed in increasing record/coordinate order. Topic selection
  uses rejection sampling for unbiased indices; each chunk's selected Topic
  index modulo64 selects its centroid. No engine RNG/distance/embedding helper
  enters expected-value code. The thin driver lives in the existing workspace
  tooling package, whose dependencies already include bench/oracle/rand; the
  generator takes a named-word-stream factory, adding no Cargo dependency.
- Name: `<kind>:<decimal-index>:a` padded with `x` to exactly48 ASCII bytes;
  state B uses `b` in that suffix. All nodes include name, active:Bool(true) and
  quality:F64(0.5); meetings add timestamp:I64, chunks ordinal/topic:I64 and
  256-byte excerpt generated from `evidence:<chunk-index>:` plus `x` padding.
  Every tenth node has aliases:StringList; every twentieth has an explicitly
  typed empty StringList, the remainder has ["alpha","beta"]. Separate small
  fixtures include the untyped EmptyList sentinel and original IEEE edge bits.
- Text starts with `amber cedar`, then alternates `cobalt delta` until no whole
  token plus separator fits; trailing bytes are spaces to the exact declared
  length. The first token becomes `quartz` for each node ordinal divisible by
  997, creating a pinned rare cohort. Whitespace-only chunks contain32 spaces
  (the plan does not specify their positive length). Shared even indices have
  48-byte text and odd indices have none. State B vector updates keep the
  original centroid and use fresh state-b-vector-noise with the same7/8:1/8
  blend, ordered f64 normalization and one f32 cast; changed bytes are checked.
  State B changes `cobalt` to `velvet`
  (same6-byte width), preserving empty/whitespace/absence and membership.
  All vocabulary is lowercase ASCII with no stemming/stopword ambiguity; the
  small golden checks the actual pinned analyzer separately from oracle logic.
- Relationship ordinal is its global ordinal:I64; weight is finite F64(0.5).
  Ordinals divisible by3 have both properties; other relationships have none.
  State-B selected relationship updates use ordinal unchanged and weight1.5,
  while recreate uses the same final properties/endpoints/type and fresh ID.
- State-B selection uses Fisher-Yates permutation with unbiased draw ranges;
  truncate to the stated count, then stable-partition selected hub records
  first (project0 node and records touching its meetings/project), keeping
  original permutation order within each partition. Selection positions for
  even/odd relationship behavior are this final order. Node updates precede
  relationship updates. Updates use revision2. Delete uses revision2; recreate
  uses revision3 and expected deletion2 in a later batch. No batch contains a
  duplicate key; paired deletes and recreates are separate <=128-item batches.
  Record every logical batch, expected key outcomes and intended checkpoint/
  consolidation barrier. Apply a barrier after all but the final batch, leaving
  exactly that last bounded batch as the active tail. Record64-envelope/16MiB
  as engine triggers, not invented actual physical byte/sync observations.
- Query cases i=0..99: i mod10 chooses hub(0), empty(1), sparse(2), ordinary
  (3..9). Hub uses project0/person0. Other projects are
  1+(17*i mod(project_count-1)); a matching meeting's primary participant gives
  a nonempty intersection. Sparse chooses a person having the smallest positive
  intersection in that project; empty chooses the first existing person absent
  from all its participants. Query centroid is i mod64, query noise independent.
  Lexical i mod4 cohorts are rare `quartz`, common `amber`, phrase `amber cedar`,
  and unmatched `zephyr`. Record actual domain/cardinality/cohort status;
  empty/no-match/special results cannot count as normal timing samples.
- Project-evidence order is descending timestamp then ascending full item,
  meeting and chunk IDs before limit100. Other ascending/full-ID tie rules and
  projections remain as the plan. Every six named query has a typed primitive
  request, exact complete logical rows/membership and bounded top-k comparator.
  Vector squared-L2 uses ordered scalar f64 accumulation over supplied f32
  values; numeric tolerance versus product f32 scores is explicit, IDs/ties
  remain exact. BM25 uses the declared analyzer's independently tokenized
  fixture subset and full-live N/df/average length. Hybrid uses Store policy v1
  fixed zero anchors, full-live vector norm enclosure/lexical maximum, explicit
  alpha0.75 with rule shifts disabled; absent modalities stay distinguishable.

## Interfaces and proof scope

Bench module emits one bounded initial preload/meeting or state-B batch at a time to a sink, plus
little-endian vector files and a JSON manifest. It computes actual topology,
full in/out/incident-degree histograms and optional/search-population counts
without retaining the complete graph or vector matrix. Small fixture mode is
explicitly `small-correctness`, never mislabeled approved baseline or stress.
File SHA-256, source/config/seed/PRNG versions, complete query parameters,
projections and state schedule are recorded; validation rejects missing/changed
files, wrong counts or incompatible versions. Large vector generation/truth is
offline and never part of measured product requests.

Std-only oracle accepts primitive keyed nodes/edges/payloads and observed full
IDs, maintains independent batch/revision/incarnation/provenance state, filters
edge liveness by both endpoints, and computes bag/path/query truth. It imports
neither benchmark generator nor engine helpers. Small self/parallel/high-ID/
restriction/recreation fixtures provide complete literal row goldens. The
production adapter and performance campaign remain later tickets.

Agreed test seams requested: public generator sink/files/manifest validation,
independent primitive oracle apply/query/compare, and existing seeded helper
sequence compatibility. Tests observe missing-edge, narrowed-high-ID,
resurrected-key and multiplicity comparator RED; generator normalization bytes
have fixed goldens; baseline/10x topology inventories are actually enumerated.
No broad product campaign, hidden product runtime or new third-party dependency.

Coordinator reviewed and approved the deterministic choices and exact state-B
vector recipe above. A proposed128-change initial-ingest refinement was
withdrawn on rereading the plan:256-node preload and137-change meeting batches
are preserved;128 applies only to state B. The manifest records
full-live anchor configuration and alpha/rule flags; this fixed query policy
does not qualify other policy configurations. The separately required
cypher-meeting-metadata seventh timing gate remains later language/qualification
work and is not silently replaced by the six named read-query expectations.

Store-v1 review clarification: the reference uses original supplied f32 vectors
from the full live vector population. For d dimensions, e = 2*(d*EPS)/(1-d*EPS),
where EPS is f64::EPSILON. Each row's upper norm is
next_up(sqrt(ordered_f64_sum_of_squares/(1-e))); take their maximum. The query
uses the same upper-norm formula. The distance upper anchor is
next_up((query_upper + document_upper)^2 * (1+e)), with lower anchor zero.
Independent literal query controls pin the rounding and an outside-eligible
row's effect on the anchor. Numeric comparison tolerance cannot replace this
policy or relax ID/rank/multiplicity comparisons.
