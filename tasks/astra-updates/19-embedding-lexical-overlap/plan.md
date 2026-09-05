# 19. Start lexical retrieval while the query embedding runs

Status: implemented and validated in this commit. Five core and three public
TextStore tests, the directed fault plant/restored control and native public
materialization case pass. All 66 ordinary and 18 diagnostic benchmark processes
pass: graph hybrid p50 improves 34.57%, p95 18.28%, with identical full payloads
and work. Actual native/lexical overlap occurs in all 576 after observations.
See [evidence](../../evidence/astra-19-embedding-overlap.md). The user requested
stopping this roadmap after 19; Steps 20–33 remain unexecuted. Broad assembled
qualification is separate and was not run.

- Stage: Queueing and overlap.
- Execution class: Required.
- Prerequisites: [07](../07-pinned-result-materialization/plan.md), [18](../18-query-cancellation/plan.md).
- Intended commit subject: Overlap lexical retrieval with query embedding.
- Shared requirements: [execution and validation contract](../README.md).

## Problem and intended result

Text hybrid embeds first and then launches parallel retrieval legs. Lexical
retrieval depends on the text and pinned lexical inputs, not the query embedding.
Overlap it with embedding using plan 07's scoped admission.

## Implementation

Own text/ingest.rs and a narrow Store hybrid prepared-execution seam.

1. Analyze the lexical query, admit/pin the Store snapshot and generation once,
   and submit its lexical producer before waiting on embedding.
2. Embed using the same query control/epoch. Submit vector retrieval only after
   validating the returned vector against the admitted dimensions/epoch.
3. Feed completed producers to existing complete-cross-fill/fusion code; preserve
   fixed anchors and iterative widening behavior.
4. Join the lexical job on every embedding error, panic, cancel and close path.
   Keep caller-owned pure fusion execution semantics unchanged.
5. Keep dense-only and lexical-only routes straightforward, with no unused worker
   or model invocation. Materialize through the original admission.

## RED -> GREEN

Add astra_19_lexical_starts_before_embedding_completion with deterministic barriers;
the current sequencing must produce the intended RED without a deadlocking test.
Add astra_19_overlap_uses_one_snapshot_despite_concurrent_ingest,
astra_19_embed_failure_joins_lexical_work,
astra_19_overlapped_hybrid_matches_serial_scores_and_text and
astra_19_single_leg_queries_execute_only_requested_leg.

## Measurements and acceptance

Record actual overlapping spans, end-to-end p50/p95 and queue wait from plan 00.
The dependency target is max(lexical, embedding + vector), followed by fusion and
materialization. Do not derive latency by subtracting independently measured
medians. Preserve work counts, score bits and reported generation.

## Validation and commit boundary

Exercise overlap start order, one-generation results and embedding-error joins using fake-runtime barriers first. Run the affected public native query case once if native code or ownership changed.

Per commit: run this plan's named RED -> GREEN tests and only existing regression
cases that cover a changed contract not covered by those tests. The targets
below are entry points, not a requirement to run each complete test binary.
If a fault/ordering/error contract changes, run its directed adversarial probe
and same-seed clean control before/after; do not run a family smoke, campaign,
whole adversarial target or workspace suite for this commit.

Keep this commit within the implementation boundary above. Do not combine another numbered plan or unrelated cleanup.
Evidence: tasks/evidence/astra-19-embedding-overlap.md. Use the shared checkpoint policy in ../README.md;
focused GREEN is not full-suite qualification.

## Executed focused test entry points

The implemented tests live at the actual library seams. The original
integration-target suggestions were planning entry points, not execution
receipts. These commands run the five core and three TextStore cases:

    cargo test -p zeppelin-embed --lib lifecycle::hybrid_overlap_tests::astra_19_ -- --nocapture
    cargo test -p zeppelin-embed-text --lib --features test-support ingest::query_overlap_tests::astra_19_ -- --nocapture

The evidence includes the exact directed-adversarial and native public commands,
literal RED/GREEN receipts, final formatting/Clippy results and measured source
hashes. No full workspace or adversarial campaign was run for this commit.
