# ZE-129 independent adjacency and liveness oracle

Status: isolated implementation candidate from exact main
`0e0064ddb3b1630e8c18050cb78c30ac7bcfb638` on
`codex/ze-129-adjacency-oracle`.

This is a std-only primitive expected-value model for ZE-44. It consumes
already-normalized changed batches and keeps full-width node/relationship IDs,
node liveness and raw authoritative relationships. It independently derives
raw OUT/IN rows, visible relationship and directional rows, visible counts,
typed degrees and planned half-open limited ranges. Visibility checks both
endpoints before capacity. Exact comparison retains and checks the supplied row
order; it never sorts or repairs an observed production result.

The [interface proposal](interface-proposal.md) was confirmed by the ZE-44
production owner before implementation. The later source review refined one
point to match the actual Cypher overlay: relationship `PropertyOnly` admission
uses pre-batch endpoint visibility, so it may coexist with same-batch DETACH and
leave a changed raw relationship that is no longer visible. Fresh relationship
creation still requires final live endpoints. Plain DELETE checks pre-batch
live incidents after excluding only explicit same-batch relationship deletes;
deleting the other endpoint never authorizes a surviving raw edge.

## Literal RED to GREEN

The focused test binary was built and run after each vertical slice.

| Slice | RED | GREEN |
| --- | --- | --- |
| Same-batch new endpoints and exact full-width rows | `same_batch_new_endpoints_create_exact_raw_and_visible_rows`: E0432 missing `graph_adjacency_store`, exit 101 | Exact raw/visible relationship and OUT/IN literals passed, nextest run `28be6e13-f035-4b7b-8753-04c82c687876` |
| Plain DELETE, self-loop and parallel incidents | `restrict_delete_requires_every_live_self_and_parallel_edge_to_be_deleted`: expected `incident_relationship`, received `unsupported_operation`, exit 100 | Explicit removal of every live incident permits deletion; a surviving self/parallel row rejects, run `36ff6e06-8feb-40cb-9f3b-12c9dd84a3e5` |
| DETACH liveness | `detach_keeps_high_degree_raw_rows_and_property_edit_uses_admitted_visibility`: visible relationships remained after DETACH, exit 100 | 4,097 raw relationship/OUT/IN rows retained and every visible stream empty, run `d25e934b-65a2-43d3-8026-fd04c3918d1c` |
| Visible limited ranges and degrees | `visible_ranges_filter_both_endpoints_before_capacity_and_count_degrees`: `unsupported_plan`, exit 100 | Dead early rows filtered before capacity one; visible count and typed OUT/IN degree passed, run `05fe184f-8ced-4f06-aaee-41179130f6ac` |
| Exact comparator | `exact_comparator_detects_direction_identity_topology_type_and_multiplicity`: E0599 missing `Snapshot::check`, exit 101 | Exact field/length/order comparison passed every positive and negative observation, run `c4183b07-358c-4c57-87dc-c5ec8dc9be6b` |

The first strict lint attempt found two test-only boolean comparison style
errors after all eight behavioral tests passed. Replacing those assertions did
not change model behavior. The terminal focused run and strict lint are both
green:

```text
cargo nextest run -p zeppelin-embed-adversarial-oracle --test graph_adjacency_store
Nextest run ID a91386b1-869f-44de-8b2f-5aa23b52283b
8 tests run: 8 passed, 0 skipped; exit 0

cargo clippy -p zeppelin-embed-adversarial-oracle --all-targets --no-deps -- -D warnings
Finished dev profile; exit 0

rustfmt --edition 2024 --check \
  tests/adversarial-oracle/src/graph_adjacency_store.rs \
  tests/adversarial-oracle/tests/graph_adjacency_store.rs
exit 0

cargo tree -p zeppelin-embed-adversarial-oracle --edges normal,build,dev
zeppelin-embed-adversarial-oracle v0.4.2 (.../tests/adversarial-oracle)
exit 0; no dependencies

git diff --check
exit 0
```

Host: Apple arm64, macOS 27.0 build 26A5388g; rustc 1.93.0;
cargo-nextest 0.9.145.

## Deterministic fixtures and controls

The eight named tests cover:

- same-batch nodes used by a new relationship and rejection when a fresh edge's
  endpoint is deleted in that batch;
- plain DELETE with a self-loop, parallel edges, both endpoint nodes pending
  deletion, explicit edge deletion and an already-dead far endpoint;
- 4,097-edge DETACH without degree enumeration/rejection, raw row retention,
  property-only preservation and explicit raw cleanup after DETACH;
- relationship IDs `1`, values above bit 100 and `u128::MAX`, high-bit node IDs,
  exact OUT/IN key order, half-open boundaries, unbounded ends and capacities
  zero, one and two;
- endpoint filtering before capacity, visible count, type-filtered degree and
  both directional degree queries;
- all nodes dead while raw relationships remain, then immutable earlier
  snapshots with their original visible data.

The comparator controls alter one observed fact at a time and assert the exact
first-difference path. They remove the reverse row, retain deleted raw authority,
retain deleted OUT and IN rows separately, truncate high relationship bits,
change an endpoint, change the relationship type, remove self-loop and parallel
multiplicity, reorder raw relationships, substitute a dead far endpoint into a
capacity-one range and compare a later state against an earlier snapshot.

## Scope boundary

This evidence proves only the primitive model and exact comparator. ZE-44 owns
the actual native producer, observer, real OUT/IN/directory agreement, physical
splits/consolidation, failure handling and seeded runner wiring. This does not
prove storage admission, codecs, allocation accounting, immutable artifacts,
publication, durability, recovery or public traversal. Broad workspace,
adversarial and per-crate coverage qualification remains ZE-118 and was not run.

All 45 inherited local-file hashes in `/tmp/ze-129-preservation.json` matched
after implementation. The existing `graph_adjacency::check_paired` source was
unchanged; its SHA-256 remains
`c8e97624db4f0af90d0e2b0f91c62d402ee49095d38f994e641f9cb8ab748868`.
