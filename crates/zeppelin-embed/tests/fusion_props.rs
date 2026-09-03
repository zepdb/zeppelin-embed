#![allow(clippy::expect_used, clippy::indexing_slicing, clippy::panic)]

mod test_support;

use proptest::prelude::*;
use proptest::test_runner::{Config, RngSeed, TestRunner};
use rand::RngCore;
use zeppelin_embed::fusion::{
    DEFAULT_ALPHA, DEFAULT_MAX_ROUNDS, DegenerateKind, DegenerateLeg, FusionError, FusionLeg,
    FusionMethod, FusionRule, FusionTermination, HYBRID_WINDOW_FLOOR, HYBRID_WINDOW_PER_K,
    HybridQuery, LEXICAL_RULE_ALPHA, LegBounds, LegFailureKind, LexicalBounds, LexicalCandidate,
    RARE_DOCUMENT_FREQUENCY_THRESHOLD, RRF_K, RuleSignals, ScorePrecision, VectorBounds,
    VectorCandidate, execute_hybrid, fuse, fuse_bounded,
};

fn proptest_cases() -> u32 {
    std::env::var("PROPTEST_CASES")
        .ok()
        .and_then(|value| value.parse().ok())
        .unwrap_or(256)
}

/// Pins the fusion policy so a value change cannot masquerade as a
/// refactor. `DEFAULT_ALPHA`, `HYBRID_WINDOW_FLOOR`, and the rules-off
/// default are measured (`tasks/evidence/17-fusion.md`, PLAN.md §A.2); the
/// rule constants are opt-in values measured harmful on SciFact; `RRF_K`,
/// `DEFAULT_MAX_ROUNDS`, and `HYBRID_WINDOW_PER_K` remain unmeasured
/// placeholders.
#[test]
fn fusion_policy_constants_are_pinned_to_their_measured_or_placeholder_values() {
    assert_eq!(DEFAULT_ALPHA.to_bits(), 0.7_f64.to_bits());
    assert_eq!(LEXICAL_RULE_ALPHA.to_bits(), 0.4_f64.to_bits());
    assert_eq!(RARE_DOCUMENT_FREQUENCY_THRESHOLD, 5);
    assert_eq!(RRF_K, 60);
    assert_eq!(DEFAULT_MAX_ROUNDS, 8);
    assert_eq!(HYBRID_WINDOW_FLOOR, 50);
    assert_eq!(HYBRID_WINDOW_PER_K, 5);
    assert_eq!(zeppelin_embed::fusion::ALPHA_POLICY_VERSION, 2);
    assert!(
        !HybridQuery::new(1).rules_enabled,
        "policy version 2: query-shape rules are opt-in"
    );
}

/// A bounded producer that contradicts its own window is a defect, not a
/// range to be repaired: the normalization it feeds would be silently wrong
/// for every hit. Each violation names the leg that supplied it.
#[test]
fn bounded_fusion_rejects_bounds_that_contradict_the_window() {
    let vector = vec![
        VectorCandidate::exact(1_u32, 0.25),
        VectorCandidate::exact(2, 0.75),
    ];
    let lexical = vec![
        LexicalCandidate::new(1_u32, 4.0),
        LexicalCandidate::new(2, 1.0),
    ];
    let query = HybridQuery::new(2);
    let sound_lexical = LexicalBounds {
        max_bm25: 4.0,
        min_bm25: 0.5,
        next_unseen_bm25: Some(0.75),
    };
    let sound_vector = VectorBounds {
        min_squared_l2: 0.25,
        max_squared_l2: 2.0,
        next_unseen_squared_l2: Some(1.5),
    };
    let cases: Vec<(LegBounds, FusionLeg, &str)> = vec![
        (
            LegBounds {
                vector: Some(VectorBounds {
                    max_squared_l2: f64::INFINITY,
                    ..sound_vector
                }),
                lexical: Some(sound_lexical),
            },
            FusionLeg::Vector,
            "a squared-L2 extreme is not finite",
        ),
        (
            LegBounds {
                // The window holds 0.75, which this range excludes.
                vector: Some(VectorBounds {
                    max_squared_l2: 0.5,
                    ..sound_vector
                }),
                lexical: Some(sound_lexical),
            },
            FusionLeg::Vector,
            "a window score lies outside the supplied extremes",
        ),
        (
            LegBounds {
                vector: Some(VectorBounds {
                    next_unseen_squared_l2: Some(0.1),
                    ..sound_vector
                }),
                lexical: Some(sound_lexical),
            },
            FusionLeg::Vector,
            "the unseen bound lies outside the supplied extremes",
        ),
        (
            LegBounds {
                vector: Some(sound_vector),
                lexical: Some(LexicalBounds {
                    min_bm25: 6.0,
                    ..sound_lexical
                }),
            },
            FusionLeg::Lexical,
            "the BM25 extremes are inverted",
        ),
        (
            LegBounds {
                vector: Some(sound_vector),
                lexical: None,
            },
            FusionLeg::Lexical,
            "a non-empty window supplied no extremes",
        ),
    ];
    for (bounds, leg, detail) in cases {
        assert_eq!(
            fuse_bounded(
                &query,
                &vector,
                &lexical,
                bounds,
                |id: &u32| Some(*id),
                |id: &u32| Some(*id),
            ),
            Err::<zeppelin_embed::fusion::FusionOutcome<u32>, _>(FusionError::InvalidBounds {
                leg,
                detail
            })
        );
    }
}

fn offline_cc(vector: &[(u32, f64)], lexical: &[(u32, f64)], alpha: f64, k: usize) -> Vec<u32> {
    let vector_min = vector
        .iter()
        .map(|(_, score)| *score)
        .fold(f64::INFINITY, f64::min);
    let vector_max = vector
        .iter()
        .map(|(_, score)| *score)
        .fold(f64::NEG_INFINITY, f64::max);
    let lexical_min = lexical
        .iter()
        .map(|(_, score)| *score)
        .fold(f64::INFINITY, f64::min);
    let lexical_max = lexical
        .iter()
        .map(|(_, score)| *score)
        .fold(f64::NEG_INFINITY, f64::max);
    let mut scores = std::collections::BTreeMap::<u32, f64>::new();
    for (key, score) in vector {
        let normalized = (vector_max - score) / (vector_max - vector_min);
        *scores.entry(*key).or_default() += alpha * normalized;
    }
    for (key, score) in lexical {
        let normalized = (score - lexical_min) / (lexical_max - lexical_min);
        *scores.entry(*key).or_default() += (1.0 - alpha) * normalized;
    }
    let mut ranked = scores.into_iter().collect::<Vec<_>>();
    ranked.sort_by(|left, right| {
        right
            .1
            .total_cmp(&left.1)
            .then_with(|| left.0.cmp(&right.0))
    });
    ranked.truncate(k.min(ranked.len()));
    ranked.into_iter().map(|(key, _)| key).collect()
}

#[test]
fn iterator_fusion_equals_fusing_the_complete_score_lists_offline() {
    let mut seed_rng = test_support::seeded_rng(
        "fusion_props::iterator_fusion_equals_fusing_the_complete_score_lists_offline",
    );
    let mut runner = TestRunner::new(Config {
        cases: proptest_cases(),
        rng_seed: RngSeed::Fixed(seed_rng.next_u64()),
        ..Config::default()
    });
    let strategy = (2_usize..40).prop_flat_map(|len| {
        (
            Just(len),
            prop::collection::vec(0_u16..10_000, len),
            prop::collection::vec(0_u16..10_000, len),
            1_usize..12,
            1_u16..999,
        )
    });
    runner
        .run(
            &strategy,
            |(len, vector_raw, lexical_raw, k, alpha_thousandths)| {
                let mut vector = vector_raw
                    .into_iter()
                    .take(len)
                    .enumerate()
                    .map(|(key, score)| (key as u32, f64::from(score)))
                    .collect::<Vec<_>>();
                let mut lexical = lexical_raw
                    .into_iter()
                    .take(len)
                    .enumerate()
                    .map(|(key, score)| (key as u32, f64::from(score)))
                    .collect::<Vec<_>>();
                prop_assume!(
                    vector.iter().any(|hit| hit.1 != vector[0].1)
                        && lexical.iter().any(|hit| hit.1 != lexical[0].1)
                );
                vector.sort_by(|left, right| left.1.total_cmp(&right.1).then(left.0.cmp(&right.0)));
                lexical
                    .sort_by(|left, right| right.1.total_cmp(&left.1).then(left.0.cmp(&right.0)));
                let vector_candidates = vector
                    .iter()
                    .map(|(key, score)| VectorCandidate::exact(*key, *score))
                    .collect::<Vec<_>>();
                let lexical_candidates = lexical
                    .iter()
                    .map(|(key, score)| LexicalCandidate::new(*key, *score))
                    .collect::<Vec<_>>();
                let alpha = f64::from(alpha_thousandths) / 1_000.0;
                let query = HybridQuery::new(k).with_alpha(alpha);
                let actual = fuse(
                    &query,
                    &vector_candidates,
                    &lexical_candidates,
                    |key| Some(*key),
                    |key| Some(*key),
                )
                .expect("valid exact score lists");
                let actual_keys = actual.hits.iter().map(|hit| hit.key).collect::<Vec<_>>();
                prop_assert_eq!(actual_keys, offline_cc(&vector, &lexical, alpha, k));
                Ok(())
            },
        )
        .expect("fusion property");
}

#[test]
fn alpha_one_reproduces_dense_order_and_alpha_zero_reproduces_lexical_order() {
    let vector = vec![
        VectorCandidate::exact('a', 0.0),
        VectorCandidate::exact('b', 2.0),
        VectorCandidate::exact('c', 8.0),
    ];
    let lexical = vec![
        LexicalCandidate::new('c', 9.0),
        LexicalCandidate::new('b', 5.0),
        LexicalCandidate::new('a', 1.0),
    ];
    let dense = fuse(
        &HybridQuery::new(3).with_alpha(1.0),
        &vector,
        &lexical,
        |key| Some(*key),
        |key| Some(*key),
    )
    .expect("alpha one");
    let lexical_only = fuse(
        &HybridQuery::new(3).with_alpha(0.0),
        &vector,
        &lexical,
        |key| Some(*key),
        |key| Some(*key),
    )
    .expect("alpha zero");

    assert_eq!(
        dense.hits.iter().map(|hit| hit.key).collect::<Vec<_>>(),
        ['a', 'b', 'c']
    );
    assert_eq!(
        lexical_only
            .hits
            .iter()
            .map(|hit| hit.key)
            .collect::<Vec<_>>(),
        ['c', 'b', 'a']
    );
}

#[test]
fn affine_transforming_one_legs_raw_scores_never_changes_the_fused_order() {
    let mut seed_rng = test_support::seeded_rng(
        "fusion_props::affine_transforming_one_legs_raw_scores_never_changes_the_fused_order",
    );
    let mut runner = TestRunner::new(Config {
        cases: proptest_cases(),
        rng_seed: RngSeed::Fixed(seed_rng.next_u64()),
        ..Config::default()
    });
    let strategy = (
        prop::collection::vec(0_u16..10_000, 3..32),
        prop::collection::vec(0_u16..10_000, 3..32),
        1_u16..100,
        0_u16..1_000,
        1_u16..999,
    );
    runner
        .run(
            &strategy,
            |(vector_raw, lexical_raw, scale, shift, alpha_raw)| {
                let len = vector_raw.len().min(lexical_raw.len());
                prop_assume!(len >= 3);
                let mut vector = vector_raw
                    .into_iter()
                    .take(len)
                    .enumerate()
                    .map(|(key, score)| (key as u32, f64::from(score)))
                    .collect::<Vec<_>>();
                let mut lexical = lexical_raw
                    .into_iter()
                    .take(len)
                    .enumerate()
                    .map(|(key, score)| (key as u32, f64::from(score)))
                    .collect::<Vec<_>>();
                prop_assume!(
                    vector.iter().any(|hit| hit.1 != vector[0].1)
                        && lexical.iter().any(|hit| hit.1 != lexical[0].1)
                );
                vector.sort_by(|left, right| left.1.total_cmp(&right.1).then(left.0.cmp(&right.0)));
                lexical
                    .sort_by(|left, right| right.1.total_cmp(&left.1).then(left.0.cmp(&right.0)));
                let alpha = f64::from(alpha_raw) / 1_000.0;
                let query = HybridQuery::new(len).with_alpha(alpha);
                let lexical_candidates = lexical
                    .iter()
                    .map(|(key, score)| LexicalCandidate::new(*key, *score))
                    .collect::<Vec<_>>();
                let original = vector
                    .iter()
                    .map(|(key, score)| VectorCandidate::exact(*key, *score))
                    .collect::<Vec<_>>();
                let transformed = vector
                    .iter()
                    .map(|(key, score)| {
                        VectorCandidate::exact(*key, *score * f64::from(scale) + f64::from(shift))
                    })
                    .collect::<Vec<_>>();
                let before = fuse(
                    &query,
                    &original,
                    &lexical_candidates,
                    |key| Some(*key),
                    |key| Some(*key),
                )
                .expect("original fusion");
                let after = fuse(
                    &query,
                    &transformed,
                    &lexical_candidates,
                    |key| Some(*key),
                    |key| Some(*key),
                )
                .expect("affine fusion");
                prop_assert_eq!(
                    before.hits.iter().map(|hit| hit.key).collect::<Vec<_>>(),
                    after.hits.iter().map(|hit| hit.key).collect::<Vec<_>>()
                );
                Ok(())
            },
        )
        .expect("affine invariance property");
}

#[test]
fn degenerate_legs_take_the_rrf_fallback_and_the_report_says_so() {
    let lexical = vec![
        LexicalCandidate::new(1_u32, 4.0),
        LexicalCandidate::new(2_u32, 1.0),
    ];
    let cases = [
        (
            Vec::new(),
            lexical.clone(),
            vec![DegenerateLeg {
                leg: FusionLeg::Vector,
                kind: DegenerateKind::Empty,
            }],
        ),
        (
            vec![VectorCandidate::exact(1_u32, 0.0)],
            lexical.clone(),
            vec![DegenerateLeg {
                leg: FusionLeg::Vector,
                kind: DegenerateKind::SingleHit,
            }],
        ),
        (
            vec![
                VectorCandidate::exact(1_u32, 3.0),
                VectorCandidate::exact(2_u32, 3.0),
            ],
            lexical,
            vec![DegenerateLeg {
                leg: FusionLeg::Vector,
                kind: DegenerateKind::AllScoresEqual,
            }],
        ),
    ];
    for (vector, lexical, reasons) in cases {
        let outcome = fuse(
            &HybridQuery::new(2),
            &vector,
            &lexical,
            |key| Some(*key),
            |key| Some(*key),
        )
        .expect("degenerate fusion");
        assert_eq!(outcome.report.method, FusionMethod::ReciprocalRankFusion);
        assert_eq!(outcome.report.degenerate_legs, reasons);
        assert!(outcome.hits.iter().all(|hit| hit.fused_score.is_finite()));
    }

    let empty = fuse(
        &HybridQuery::new(10),
        &Vec::<VectorCandidate<u32>>::new(),
        &Vec::<LexicalCandidate<u32>>::new(),
        |key| Some(*key),
        |key| Some(*key),
    )
    .expect("empty fusion");
    assert!(empty.hits.is_empty());
    assert_eq!(empty.report.method, FusionMethod::ReciprocalRankFusion);
    assert_eq!(
        empty.report.degenerate_legs,
        [
            DegenerateLeg {
                leg: FusionLeg::Vector,
                kind: DegenerateKind::Empty,
            },
            DegenerateLeg {
                leg: FusionLeg::Lexical,
                kind: DegenerateKind::Empty,
            },
        ]
    );
}

#[test]
fn a_quoted_phrase_query_shifts_alpha_and_the_report_names_the_rule() {
    let vector = vec![
        VectorCandidate::exact(1_u32, 0.0),
        VectorCandidate::exact(2_u32, 1.0),
        VectorCandidate::exact(3_u32, 4.0),
    ];
    let lexical = vec![
        LexicalCandidate::new(3_u32, 8.0),
        LexicalCandidate::new(2_u32, 4.0),
        LexicalCandidate::new(1_u32, 1.0),
    ];
    let signals = RuleSignals {
        quoted_phrase: true,
        rarest_exact_document_frequency: None,
        identifier_token: true,
    };
    let shifted = fuse(
        &HybridQuery::new(3).with_rules().with_rule_signals(signals),
        &vector,
        &lexical,
        |key| Some(*key),
        |key| Some(*key),
    )
    .expect("rule-shifted fusion");
    assert_eq!(shifted.report.effective_alpha, LEXICAL_RULE_ALPHA);
    assert_eq!(
        shifted.report.applied_rules,
        [FusionRule::QuotedPhrase, FusionRule::IdentifierToken]
    );

    let rare = fuse(
        &HybridQuery::new(3)
            .with_rules()
            .with_rule_signals(RuleSignals {
                quoted_phrase: false,
                rarest_exact_document_frequency: Some(1),
                identifier_token: false,
            }),
        &vector,
        &lexical,
        |key| Some(*key),
        |key| Some(*key),
    )
    .expect("rare-token fusion");
    assert_eq!(rare.report.effective_alpha, LEXICAL_RULE_ALPHA);
    assert_eq!(rare.report.applied_rules, [FusionRule::RareExactToken]);

    let disabled = fuse(
        &HybridQuery::new(3)
            .with_rule_signals(signals)
            .without_rules(),
        &vector,
        &lexical,
        |key| Some(*key),
        |key| Some(*key),
    )
    .expect("rules disabled");
    assert_eq!(disabled.report.effective_alpha, DEFAULT_ALPHA);
    assert!(disabled.report.applied_rules.is_empty());
}

#[test]
fn tied_input_iteration_order_never_changes_the_fused_result() {
    let vector = vec![
        VectorCandidate::exact('a', 2.0),
        VectorCandidate::exact('b', 2.0),
        VectorCandidate::exact('c', 2.0),
    ];
    let lexical = vec![
        LexicalCandidate::new('a', 5.0),
        LexicalCandidate::new('b', 5.0),
        LexicalCandidate::new('c', 5.0),
    ];
    let mut reversed_vector = vector.clone();
    let mut reversed_lexical = lexical.clone();
    reversed_vector.reverse();
    reversed_lexical.reverse();
    let forward = fuse(
        &HybridQuery::new(3),
        &vector,
        &lexical,
        |key| Some(*key),
        |key| Some(*key),
    )
    .expect("forward ties");
    let reversed = fuse(
        &HybridQuery::new(3),
        &reversed_vector,
        &reversed_lexical,
        |key| Some(*key),
        |key| Some(*key),
    )
    .expect("reversed ties");
    assert_eq!(forward, reversed);
}

#[test]
fn validation_rejects_every_invalid_leg_shape_at_its_exact_rank() {
    let valid_vector = [
        VectorCandidate::exact(1_u32, 0.0),
        VectorCandidate::exact(2_u32, 1.0),
    ];
    let valid_lexical = [
        LexicalCandidate::new(1_u32, 2.0),
        LexicalCandidate::new(2_u32, 1.0),
    ];
    let run = |vector: &[VectorCandidate<u32>], lexical: &[LexicalCandidate<u32>]| {
        fuse(
            &HybridQuery::new(2).with_alpha(0.5),
            vector,
            lexical,
            |key| Some(*key),
            |key| Some(*key),
        )
    };
    let cases = [
        (
            run(&[VectorCandidate::exact(1, f64::NAN)], &valid_lexical),
            FusionError::NonFiniteScore {
                leg: FusionLeg::Vector,
                rank: 0,
            },
        ),
        (
            run(&[VectorCandidate::exact(1, -1.0)], &valid_lexical),
            FusionError::NegativeScore {
                leg: FusionLeg::Vector,
                rank: 0,
            },
        ),
        (
            run(
                &[
                    VectorCandidate::exact(1, 2.0),
                    VectorCandidate::exact(2, 1.0),
                ],
                &valid_lexical,
            ),
            FusionError::UnrankedInput {
                leg: FusionLeg::Vector,
                rank: 1,
            },
        ),
        (
            run(&valid_vector, &[LexicalCandidate::new(1, f64::INFINITY)]),
            FusionError::NonFiniteScore {
                leg: FusionLeg::Lexical,
                rank: 0,
            },
        ),
        (
            run(&valid_vector, &[LexicalCandidate::new(1, -1.0)]),
            FusionError::NegativeScore {
                leg: FusionLeg::Lexical,
                rank: 0,
            },
        ),
        (
            run(
                &valid_vector,
                &[LexicalCandidate::new(1, 1.0), LexicalCandidate::new(2, 2.0)],
            ),
            FusionError::UnrankedInput {
                leg: FusionLeg::Lexical,
                rank: 1,
            },
        ),
    ];
    for (actual, expected) in cases {
        assert_eq!(actual, Err(expected));
    }

    let missing_vector = fuse(
        &HybridQuery::new(2),
        &valid_vector,
        &valid_lexical,
        |_| None::<u32>,
        |key| Some(*key),
    );
    assert_eq!(
        missing_vector,
        Err(FusionError::MissingDocumentIdentity {
            leg: FusionLeg::Vector,
            rank: 0,
        })
    );
    let missing_lexical = fuse(
        &HybridQuery::new(2),
        &valid_vector,
        &valid_lexical,
        |key| Some(*key),
        |_| None::<u32>,
    );
    assert_eq!(
        missing_lexical,
        Err(FusionError::MissingDocumentIdentity {
            leg: FusionLeg::Lexical,
            rank: 0,
        })
    );
    let duplicate_vector = fuse(
        &HybridQuery::new(2),
        &valid_vector,
        &valid_lexical,
        |_| Some(1_u32),
        |key| Some(*key),
    );
    assert_eq!(
        duplicate_vector,
        Err(FusionError::DuplicateDocumentIdentity {
            leg: FusionLeg::Vector,
            rank: 1,
        })
    );
    let duplicate_lexical = fuse(
        &HybridQuery::new(2),
        &valid_vector,
        &valid_lexical,
        |key| Some(*key),
        |_| Some(1_u32),
    );
    assert_eq!(
        duplicate_lexical,
        Err(FusionError::DuplicateDocumentIdentity {
            leg: FusionLeg::Lexical,
            rank: 1,
        })
    );
}

#[test]
fn reports_distinguish_stability_exhaustion_and_budget_materialization() {
    let vector = [
        VectorCandidate::exact(1_u32, 0.0),
        VectorCandidate::exact(2_u32, 99.0),
        VectorCandidate::exact(3_u32, 100.0),
    ];
    let lexical = [
        LexicalCandidate::new(1_u32, 100.0),
        LexicalCandidate::new(2_u32, 1.0),
        LexicalCandidate::new(3_u32, 0.0),
    ];
    let join = |query: &HybridQuery| {
        fuse(query, &vector, &lexical, |key| Some(*key), |key| Some(*key)).expect("valid fixture")
    };
    let stable = join(&HybridQuery::new(1).with_alpha(0.5));
    assert_eq!(stable.report.termination, FusionTermination::StableBound);
    assert_eq!(stable.report.rounds, 1);
    assert!(!stable.report.budget_exhausted);

    let exhausted = join(&HybridQuery::new(3).with_alpha(0.5));
    assert_eq!(
        exhausted.report.termination,
        FusionTermination::ListsExhausted
    );
    let tokenizer = zeppelin_embed::fts::tokenizer::Profile::TextDefault
        .config()
        .epoch();
    let tower = zeppelin_embed::epoch::EmbeddingTower {
        model_id: "fusion-fixture".to_owned(),
        model_version: "1".to_owned(),
        weights_digest: vec![7],
        dims: 3,
        normalization: zeppelin_embed::epoch::Normalization::L2,
        prompt_prefix: String::new(),
        max_tokens: 32,
        runtime: zeppelin_embed::epoch::EmbeddingRuntime::CpuReference,
        compute_units: zeppelin_embed::epoch::ComputeUnits::Cpu,
        os_build: None,
    };
    let embedding = zeppelin_embed::epoch::EmbeddingEpoch {
        document: tower.clone(),
        query: tower,
        alignment_digest: Vec::new(),
    };
    let epoch = zeppelin_embed::epoch::EpochIdentity {
        embedding: zeppelin_embed::epoch::EpochId::of(&embedding),
        tokenizer,
    };
    let budget = join(
        &HybridQuery::new(1)
            .with_alpha(0.5)
            .with_max_rounds(0)
            .with_epoch(epoch),
    );
    assert_eq!(
        budget.report.termination,
        FusionTermination::BudgetFullMaterialization
    );
    assert!(budget.report.budget_exhausted);
    assert_eq!(budget.report.rounds, 0);
    assert_eq!(budget.report.epoch, Some(epoch));

    let no_hits = join(&HybridQuery::new(0).with_alpha(0.5));
    assert!(no_hits.hits.is_empty());
    assert_eq!(no_hits.report.rounds, 1);
}

#[test]
fn transparent_hits_and_accessors_preserve_native_leg_facts() {
    let exact = VectorCandidate::exact('v', 4.0);
    assert_eq!(exact.id(), &'v');
    assert_eq!(exact.squared_l2(), 4.0);
    assert_eq!(exact.precision(), ScorePrecision::Exact);
    let estimated = VectorCandidate::estimated('e', 3.0);
    assert_eq!(estimated.precision(), ScorePrecision::Estimated);
    let lexical_hit = LexicalCandidate::new('l', 2.0);
    assert_eq!(lexical_hit.id(), &'l');
    assert_eq!(lexical_hit.bm25(), 2.0);

    let outcome = fuse(
        &HybridQuery::new(4).with_alpha(0.5),
        &[
            VectorCandidate::exact('a', 0.0),
            VectorCandidate::exact('b', 1.0),
        ],
        &[
            LexicalCandidate::new('c', 2.0),
            LexicalCandidate::new('d', 1.0),
        ],
        |key| Some(*key),
        |key| Some(*key),
    )
    .expect("disjoint exact legs");
    assert!(
        outcome
            .hits
            .iter()
            .any(|hit| hit.vector_squared_l2.is_some() && hit.lexical_bm25.is_none())
    );
    assert!(
        outcome
            .hits
            .iter()
            .any(|hit| hit.vector_squared_l2.is_none() && hit.lexical_bm25.is_some())
    );
}

#[test]
fn alpha_and_leg_failures_are_typed_displayable_and_short_circuit() {
    let vector = [
        VectorCandidate::exact(1_u32, 0.0),
        VectorCandidate::exact(2_u32, 1.0),
    ];
    let lexical = [
        LexicalCandidate::new(1_u32, 2.0),
        LexicalCandidate::new(2_u32, 1.0),
    ];
    for alpha in [f64::NAN, f64::INFINITY, -0.1, 1.1] {
        let error = fuse(
            &HybridQuery::new(2).with_alpha(alpha),
            &vector,
            &lexical,
            |key| Some(*key),
            |key| Some(*key),
        )
        .expect_err("invalid alpha");
        assert!(error.to_string().contains("fusion alpha"));
    }

    let leg_errors = [
        FusionError::NonFiniteScore {
            leg: FusionLeg::Vector,
            rank: 1,
        },
        FusionError::NegativeScore {
            leg: FusionLeg::Lexical,
            rank: 2,
        },
        FusionError::EstimatedVectorScore { rank: 3 },
        FusionError::UnrankedInput {
            leg: FusionLeg::Vector,
            rank: 4,
        },
        FusionError::MissingDocumentIdentity {
            leg: FusionLeg::Lexical,
            rank: 5,
        },
        FusionError::DuplicateDocumentIdentity {
            leg: FusionLeg::Vector,
            rank: 6,
        },
        FusionError::Timeout { partial: false },
        FusionError::Cancelled { partial: false },
        FusionError::ReadCancelled { partial: false },
        FusionError::Leg {
            leg: FusionLeg::Lexical,
            kind: LegFailureKind::Invariant,
            detail: "fixture".to_owned(),
        },
    ];
    for error in leg_errors {
        assert!(!error.to_string().is_empty());
    }

    let vector_failure = execute_hybrid(
        &HybridQuery::new(1),
        || Err::<Vec<VectorCandidate<u32>>, _>(FusionError::Cancelled { partial: false }),
        || -> Result<Vec<LexicalCandidate<u32>>, FusionError> {
            panic!("lexical leg must not run after vector failure")
        },
        |key| Some(*key),
        |key| Some(*key),
    );
    assert_eq!(
        vector_failure,
        Err(FusionError::Cancelled { partial: false })
    );
    let lexical_failure = execute_hybrid(
        &HybridQuery::new(1),
        || Ok(vec![VectorCandidate::exact(1_u32, 0.0)]),
        || {
            Err(FusionError::Leg {
                leg: FusionLeg::Lexical,
                kind: LegFailureKind::Caller,
                detail: "lexical fixture".to_owned(),
            })
        },
        |key| Some(*key),
        |key| Some(*key),
    );
    assert_eq!(
        lexical_failure,
        Err(FusionError::Leg {
            leg: FusionLeg::Lexical,
            kind: LegFailureKind::Caller,
            detail: "lexical fixture".to_owned(),
        })
    );

    let mapped = [
        FusionError::from(zeppelin_embed::lifecycle::QueryError::Cancelled { partial: true }),
        FusionError::from(zeppelin_embed::lifecycle::QueryError::ReadCancelled { partial: true }),
        FusionError::from(zeppelin_embed::lifecycle::QueryError::Store(
            zeppelin_embed::lifecycle::StoreError::Closed,
        )),
    ];
    assert_eq!(mapped[0], FusionError::Cancelled { partial: false });
    assert_eq!(mapped[1], FusionError::ReadCancelled { partial: false });
    assert_eq!(
        mapped[2],
        FusionError::Leg {
            leg: FusionLeg::Vector,
            kind: LegFailureKind::Store(zeppelin_embed::lifecycle::StoreErrorKind::Closed),
            detail: "store is closed".to_owned(),
        }
    );
}
