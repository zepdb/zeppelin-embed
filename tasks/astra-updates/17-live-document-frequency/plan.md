# 17. Cache exact live term frequencies for tombstoned segments

Status: implemented and retained. Nine focused tests, two directed fault plants
and the five-case after-17 integration checkpoint pass. Three preserved core
experiment matrices establish the final admission/fast-path policy; all 24 native
TextStore confirmation processes pass with identical full payloads and quality.
Broad qualification remains NOT RUN. The implementation and complete evidence
are in the commit containing this status; source identities are in the report.

- Stage: Lexical setup.
- Execution class: Required for update-heavy workloads.
- Prerequisites: [16](../16-incremental-lexical-assembly/plan.md).
- Intended commit subject: Cache live lexical document frequencies by immutable inputs.
- Shared requirements: [execution and validation contract](../README.md).

## Problem and intended result

Any tombstones cause live_document_frequency to walk a queried term's doc IDs.
Retrieval then walks postings again. This work is often invisible in ordinary
retrieval counters and repeats on unchanged immutable inputs.

## Implementation

Own fts/index.rs, fts/sealed.rs and plan 16's per-reader contribution cache.

1. Cache exact DF by reader/alive-set identity, term bytes and canonical field
   set. Include every input that changes document membership.
2. Preserve union semantics: a document matching two fields counts once.
   Duplicate query terms do not duplicate cached DF entries or change scores.
3. Build values lazily with bounded memory admission/eviction; concurrent misses
   must not grow an unaccounted cache. Cached errors cannot become zero DF.
4. Keep the all-live dictionary fast path. Optionally count intersections using
   existing compressed structures only when they preserve the exact live model.
5. Record DF-specific doc-ID/block decode work separately from retrieval work.

## RED -> GREEN

Add astra_17_live_df_scans_once_for_repeated_immutable_term,
astra_17_live_df_field_union_is_exact,
astra_17_live_df_invalidates_on_delete_and_not_unrelated_query and
astra_17_live_df_cache_memory_released_with_reader.
Compare N, total tokens, each DF, IDF and final BM25 to an independent tombstoned
corpus model. Include zero-live, all-live, strict field subsets and retention.

## Measurements and acceptance

Warm repeated DF queries perform no postings walk for cached identities. Record
cold miss cost, warm hit rate, cache bytes and workloads with mostly unique
terms; disable/cap admission if cache overhead exceeds the measured benefit.
No approximate DF or stale global statistics are allowed.

## Validation and commit boundary

Exercise exact live DF/field unions and cache invalidation/release. Include the narrow independent BM25 stats fixture; do not rerun all lexical query operators.

Per commit: run this plan's named RED -> GREEN tests and only existing regression
cases that cover a changed contract not covered by those tests. The targets
below are entry points, not a requirement to run each complete test binary.
If a fault/ordering/error contract changes, run its directed adversarial probe
and same-seed clean control before/after; do not run a family smoke, campaign,
whole adversarial target or workspace suite for this commit.

Keep this commit within the implementation boundary above. Do not combine another numbered plan or unrelated cleanup.
Evidence: tasks/evidence/astra-17-live-df.md. Use the shared checkpoint policy in ../README.md;
focused GREEN is not full-suite qualification.

## Focused test entry points

Use only targets that actually contain new or changed tests for this plan.
Filter to one named test for RED and identical GREEN; the plan prefix below
runs the small completed set. A command executing zero tests is not evidence.

    cargo test -p zeppelin-embed --test lexical_stats_consistency astra_17_
    cargo test -p zeppelin-embed --test prune_equivalence astra_17_
    cargo test -p zeppelin-embed --test lexical_extras astra_17_

Run a specifically named existing neighbor test only when it covers an
additional affected contract. Reuse passing evidence at the same relevant code
state; report native blockers without expanding into unrelated tests.
