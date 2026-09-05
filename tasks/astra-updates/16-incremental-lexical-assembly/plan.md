# 16. Reuse sealed lexical statistics across active mutations

Status: implemented with focused GREEN and matched core measurements; committed as `87421b7`.
Native after-17 confirmation and broad qualification remain NOT RUN.

- Stage: Lexical setup.
- Execution class: Required for update-heavy workloads.
- Prerequisites: [15](../15-bitmap-validation/plan.md).
- Intended commit subject: Reuse sealed lexical assembly contributions across ingest.
- Shared requirements: [execution and validation contract](../README.md).

## Problem and intended result

The existing assembled-index cache keys the complete snapshot and active Arc.
An active mutation invalidates the entire assembly, causing unchanged sealed
segments' counters and bitmap scaffolding to be rebuilt.

## Implementation

Own lexical assembly/cache structures in lifecycle/mod.rs, fts/index.rs and
query-independent active metadata only where necessary.

1. Cache sealed contribution objects by reader identity and exact alive set.
   Include shared postings, alive view, live row/token counts and source mapping.
2. Rebuild only changed contributions, then fold store-wide N/token totals and
   assemble source ordinals for the current pinned snapshot.
3. Cache active alive/norm/lexical summaries on immutable published ActiveSegment
   instances where safe. A new write-side copy receives new identity/state;
   preserve the existing copy-on-write publication and WAL order.
4. Attach memory reservations to the Arc-owned cached value, not merely the
   replaceable cache entry, so in-flight readers remain accounted after eviction.
5. Keep cache construction bounded. Avoid stale builders repeatedly replacing
   newer entries; no unbounded history of assemblies or reader retention.

## RED -> GREEN

Add astra_16_active_ingest_reuses_unchanged_sealed_lexical_statistics,
astra_16_alive_change_invalidates_only_affected_contribution,
astra_16_cached_global_stats_match_exhaustive_live_model and
astra_16_evicted_assembly_remains_accounted_until_last_query_drops.
Exercise active replacement, delete, seal, consolidation, retention and old/new
queries overlapping via barriers. Assert no stale DocId/source mapping.

## Measurements and acceptance

Under one active mutation between queries, sealed row/token walks are zero for
unchanged inputs. Report contribution builds/hits, memory high-water and per-query
setup time. Global N/avgdl and raw BM25 remain bit-identical to uncached execution.

## Validation and commit boundary

Exercise active-update reuse and old/new snapshot cache accounting through deterministic barriers. Run only changed stale-statistics and publication-lifetime probes.

Per commit: run this plan's named RED -> GREEN tests and only existing regression
cases that cover a changed contract not covered by those tests. The targets
below are entry points, not a requirement to run each complete test binary.
If a fault/ordering/error contract changes, run its directed adversarial probe
and same-seed clean control before/after; do not run a family smoke, campaign,
whole adversarial target or workspace suite for this commit.

Keep this commit within the implementation boundary above. Do not combine another numbered plan or unrelated cleanup.
Evidence: tasks/evidence/astra-16-lexical-assembly.md. Use the shared checkpoint policy in ../README.md;
focused GREEN is not full-suite qualification.

## Focused test entry points

Use only targets that actually contain new or changed tests for this plan.
Filter to one named test for RED and identical GREEN; the plan prefix below
runs the small completed set. A command executing zero tests is not evidence.

    cargo test -p zeppelin-embed --test lexical_stats_consistency astra_16_
    cargo test -p zeppelin-embed --test store_text_columns astra_16_
    cargo test -p zeppelin-embed --test query_ready_views astra_16_

Run a specifically named existing neighbor test only when it covers an
additional affected contract. Reuse passing evidence at the same relevant code
state; report native blockers without expanding into unrelated tests.
