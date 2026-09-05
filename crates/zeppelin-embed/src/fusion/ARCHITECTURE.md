# Fusion architecture-decision ledger

This ledger describes the live module contract. Historical plans and research
priors are not substitutes for these executable invariants.

## Current invariants

| Decision | Executable evidence |
| --- | --- |
| Store-owned hybrid exists: `Store::search_hybrid` pins the vector and lexical legs to one generation, joins them by `DocId`, and routes blend policy and `FusionReport` construction through this module. | [`store_level_hybrid_default_is_exact_and_populates_diagnostics`](../../tests/store_text_columns.rs) |
| No tier preference selects exhaustive exact vector scoring for hybrid. `SearchOptions::with_tier` records an explicit preference, including explicit `Auto`, and that caller choice wins. | [`store_level_hybrid_default_is_exact_and_populates_diagnostics`, `store_level_hybrid_explicit_estimated_tier_is_rejected`, and `store_level_hybrid_explicit_exact_tier_preserves_ordered_score_bits`](../../tests/store_text_columns.rs) |
| Fusion accepts exact vector scores only. An explicit estimated tier reaches `FusionError::EstimatedVectorScore` instead of being silently blended. | [`store_level_hybrid_explicit_estimated_tier_is_rejected`](../../tests/store_text_columns.rs) |
| The pure fusion APIs use independent per-leg min-max normalization and RRF for degenerate legs. Store-owned fusion uses the separately versioned policy below. | [`iterator_fusion_equals_fusing_the_complete_score_lists_offline` and `degenerate_legs_take_the_rrf_fallback_and_the_report_says_so`](../../tests/fusion_props.rs) |

## Rejected alternatives

| Rejected alternative | Guard |
| --- | --- |
| Silently combine estimated vector scores with exact lexical scores. | [`store_level_hybrid_explicit_estimated_tier_is_rejected`](../../tests/store_text_columns.rs) |
| Weight raw cross-leg scores without per-leg normalization. | [`affine_transforming_one_legs_raw_scores_never_changes_the_fused_order`](../../tests/fusion_props.rs) |
| Invent a convex-combination value when min-max normalization is undefined. | [`degenerate_legs_take_the_rrf_fallback_and_the_report_says_so`](../../tests/fusion_props.rs) |

## Measured and unmeasured policies

`DEFAULT_ALPHA = 0.7` is MEASURED: `tasks/evidence/17-fusion.md` (BEIR
SciFact, Cohere embed-english-v3, 2026-08-26) reads the optimum off an
eleven-point grid, nDCG@10 0.7642 against 0.7181 dense-only. The query-shape
rules (`LEXICAL_RULE_ALPHA`, `RARE_DOCUMENT_FREQUENCY_THRESHOLD`) were
measured harmful at every cell of a 28-cell sweep, so policy version 2 ships
them off by default and `HybridQuery::with_rules` is the explicit opt-in.
`RRF_K` and `DEFAULT_MAX_ROUNDS` remain labeled NOT YET MEASURED.
[`fusion_policy_constants_are_pinned_to_their_measured_or_placeholder_values`](../../tests/fusion_props.rs)
pins every value and the rules-off default so a policy change cannot
masquerade as refactoring.

R04 may replace the currently materialized leg producers with bounded producers.
It must preserve every fusion invariant above; this ledger does not authorize
that producer-contract change.

## Store normalization policy v1 (Astra 03)

Store hybrid freezes anchors after both initial producers complete: vector
[0, validated norm enclosure] and lexical [0, exact maximum live combined BM25].
The enclosure is not a hardcoded constant, even for normalized text vectors.
Widening and full materialization use the same anchors. Every candidate in the
physical-row plus DocumentVersion union receives both exact raw scores; lexical
nonmembership is a computed zero. A missing score is never treated as absence.

An empty lexical leg gives the vector leg full weight; an empty vector leg gives
lexical full weight. A zero vector enclosure has constant normalized score one;
a zero lexical maximum contributes zero. Store scoring remains convex combination
for singleton and equal-score lists. Empty unions return no hits. The independent
alpha policy is unchanged. Pure complete-list/bounded APIs retain their existing
min-max/RRF rules and their separate rank-bound proofs. HybridReport records
normalization_policy_version=1 and complete structured lexical aggregations.

Exact W+1 producer boundaries bound rows outside both windows. ANN boundaries
remain approximate even after exact cross-scoring; plan 01's coverage guard still
prevents a false certificate. Structured hybrid temporarily aggregates complete
per-expansion results to obtain a valid combined maximum and full cross-scores;
plan 10 replaces that cost with exact combined top-k. No persisted or C layout
is extended by this Rust policy change.

The alpha measurements above describe the historical normalization policy. Their
quality optimum is not assumed to transfer to v1; relevance calibration and
held-out qualification remain separate work. Public fixed-policy literal/parity
and degenerate controls are in hybrid_bounded.rs; independent Store replay and
missing-score/floor/policy/round plants are in the adversarial hybrid oracle.
