# Fusion architecture-decision ledger

This ledger describes the live module contract. Historical plans and research
priors are not substitutes for these executable invariants.

## Current invariants

| Decision | Executable evidence |
| --- | --- |
| Store-owned hybrid exists: `Store::search_hybrid` pins the vector and lexical legs to one generation, joins them by `DocId`, and routes blend policy and `FusionReport` construction through this module. | [`store_level_hybrid_default_is_exact_and_populates_diagnostics`](../../tests/store_text_columns.rs) |
| No tier preference selects exhaustive exact vector scoring for hybrid. `SearchOptions::with_tier` records an explicit preference, including explicit `Auto`, and that caller choice wins. | [`store_level_hybrid_default_is_exact_and_populates_diagnostics`, `store_level_hybrid_explicit_estimated_tier_is_rejected`, and `store_level_hybrid_explicit_exact_tier_preserves_ordered_score_bits`](../../tests/store_text_columns.rs) |
| Fusion accepts exact vector scores only. An explicit estimated tier reaches `FusionError::EstimatedVectorScore` instead of being silently blended. | [`store_level_hybrid_explicit_estimated_tier_is_rejected`](../../tests/store_text_columns.rs) |
| The shipped blend is an alpha-weighted convex combination after independent per-leg min-max normalization. A degenerate leg selects reported reciprocal-rank fusion instead. | [`iterator_fusion_equals_fusing_the_complete_score_lists_offline` and `degenerate_legs_take_the_rrf_fallback_and_the_report_says_so`](../../tests/fusion_props.rs) |

## Rejected alternatives

| Rejected alternative | Guard |
| --- | --- |
| Silently combine estimated vector scores with exact lexical scores. | [`store_level_hybrid_explicit_estimated_tier_is_rejected`](../../tests/store_text_columns.rs) |
| Weight raw cross-leg scores without per-leg normalization. | [`affine_transforming_one_legs_raw_scores_never_changes_the_fused_order`](../../tests/fusion_props.rs) |
| Invent a convex-combination value when min-max normalization is undefined. | [`degenerate_legs_take_the_rrf_fallback_and_the_report_says_so`](../../tests/fusion_props.rs) |

## Unmeasured policies

`DEFAULT_ALPHA`, `LEXICAL_RULE_ALPHA`, `RARE_DOCUMENT_FREQUENCY_THRESHOLD`,
`RRF_K`, and `DEFAULT_MAX_ROUNDS` remain explicitly labeled **NOT YET
MEASURED** for a Zeppelin corpus. Their current values are shipped placeholders,
not quality claims. [`unmeasured_policy_placeholders_are_explicit`](../../tests/fusion_props.rs)
pins those values so a policy change cannot masquerade as refactoring.

R04 may replace the currently materialized leg producers with bounded producers.
It must preserve every fusion invariant above; this ledger does not authorize
that producer-contract change.
