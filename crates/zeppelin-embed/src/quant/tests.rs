#![allow(
    clippy::expect_used,
    clippy::indexing_slicing,
    clippy::panic,
    clippy::unwrap_used
)]

use proptest::collection::vec;
use proptest::prelude::*;
use proptest::test_runner::RngSeed;
use rand::Rng;

use crate::kernels::MAX_DOT_I8_DIMENSION;

use super::{
    Int8Vec, QuantError, QuantScheme, RescoreCheckError, RescoreError, RescoreMetric, RescorePool,
    dequantize_bit4, dequantize_int8, dot_int8_query, est_dot_bit4, est_dot_bit4_batch,
    prepare_bit4_query, prepare_int8_query, quantize_bit4, quantize_int8, rescore_top_k,
    rescore_top_k_with_check,
};

fn fixture_f32(path: &str) -> Vec<f32> {
    path.split_ascii_whitespace()
        .map(|value| value.parse::<f32>().expect("golden f32 is valid"))
        .collect()
}

fn fixture_hex(path: &str) -> Vec<u8> {
    path.split_ascii_whitespace()
        .map(|value| u8::from_str_radix(value, 16).expect("golden byte is valid"))
        .collect()
}

#[test]
fn persisted_scheme_ids_keep_retired_ids_reserved() {
    assert_eq!(QuantScheme::F32.id(), 0);
    assert_eq!(QuantScheme::F16.id(), 1);
    assert_eq!(QuantScheme::Int8.id(), 2);
    assert_eq!(QuantScheme::Bit4.id(), 4);

    assert_eq!(QuantScheme::from_id(0), Some(QuantScheme::F32));
    assert_eq!(QuantScheme::from_id(1), Some(QuantScheme::F16));
    assert_eq!(QuantScheme::from_id(2), Some(QuantScheme::Int8));
    assert_eq!(QuantScheme::from_id(3), None);
    assert_eq!(QuantScheme::from_id(4), Some(QuantScheme::Bit4));
    assert_eq!(QuantScheme::from_id(5), None);
    assert_eq!(QuantScheme::from_id(6), None);
    assert_eq!(QuantScheme::from_id(u8::MAX), None);
}

#[test]
fn default_quantization_scheme_is_bit4() {
    assert_eq!(QuantScheme::default(), QuantScheme::Bit4);
}

#[test]
fn narrow_row_sizes_at_dimension_768_are_pinned() {
    let input = vec![0.0_f32; 768];
    let mut int8 = vec![0_i8; input.len()];
    let mut bit4 = vec![0_u8; input.len().div_ceil(2)];

    quantize_int8(&input, &mut int8).expect("valid int8 row");
    quantize_bit4(&input, &mut bit4).expect("valid bit4 row");

    assert_eq!(int8.len(), 768);
    assert_eq!(bit4.len(), 384);
    assert_eq!(2 * std::mem::size_of::<f32>(), 8);
}

#[test]
fn bit4_golden_fixture_is_stable() {
    let input = fixture_f32(include_str!("../../tests/fixtures/quant/narrow_input.txt"));
    let expected = fixture_hex(include_str!("../../tests/fixtures/quant/bit4_code.hex"));
    let mut actual = vec![u8::MAX; expected.len()];

    let factors = quantize_bit4(&input, &mut actual).expect("fixture is finite and sized");

    assert_eq!(actual, expected);
    assert_eq!(factors.norm(), 1.0);
    assert_eq!(factors.correction(), f64::from(2.0_f32 / 15.0));
    let mut reconstructed = vec![0.0; input.len()];
    dequantize_bit4(&actual, factors, &mut reconstructed).expect("golden code decodes");
    let mut roundtrip = vec![0_u8; actual.len()];
    quantize_bit4(&reconstructed, &mut roundtrip).expect("reconstruction re-encodes");
    assert_eq!(roundtrip, expected);
}

#[test]
fn int8_golden_fixture_is_stable() {
    let input = fixture_f32(include_str!("../../tests/fixtures/quant/narrow_input.txt"));
    let expected = fixture_hex(include_str!("../../tests/fixtures/quant/int8_code.hex"));
    let mut actual = vec![0_i8; expected.len()];

    let (scale, offset) = quantize_int8(&input, &mut actual).expect("finite fixture");

    assert_eq!(
        actual.iter().map(|code| *code as u8).collect::<Vec<_>>(),
        expected
    );
    assert_eq!(scale.to_bits(), (1.0_f32 / 254.0).to_bits());
    assert_eq!(offset, 0.5);
    let mut reconstructed = vec![f32::NAN; input.len()];
    dequantize_int8(
        Int8Vec {
            codes: &actual,
            scale,
            offset,
        },
        &mut reconstructed,
    )
    .expect("matching fixture");
    let mut roundtrip = vec![0_i8; actual.len()];
    quantize_int8(&reconstructed, &mut roundtrip).expect("reconstruction re-encodes");
    assert_eq!(roundtrip, actual);
}

#[test]
fn bit4_native_estimator_scores_aligned_vector() {
    let input = fixture_f32(include_str!("../../tests/fixtures/quant/narrow_input.txt"));
    let mut codes = vec![0_u8; input.len().div_ceil(2)];
    let factors = quantize_bit4(&input, &mut codes).expect("valid row");
    let query = prepare_bit4_query(&input, 11).expect("valid query");

    let estimate = est_dot_bit4(&query, &codes, factors).expect("matching code");

    assert!((estimate - 1.0).abs() <= f32::EPSILON);
}

#[test]
fn bit4_batch_estimator_is_bit_exact_to_single_row_oracle() {
    const DIMENSION: usize = 97;
    let query_values = (0..DIMENSION)
        .map(|index| ((index as f32 * 0.173).sin() * 0.75) + 0.125)
        .collect::<Vec<_>>();
    let query = prepare_bit4_query(&query_values, 0x4ba7_c001).expect("valid query");
    let rows = [
        vec![0.0_f32; DIMENSION],
        (0..DIMENSION)
            .map(|index| (index as f32 * 0.071).cos())
            .collect(),
        (0..DIMENSION)
            .map(|index| ((index % 11) as f32 - 5.0) / 7.0)
            .collect(),
    ];
    let row_bytes = DIMENSION.div_ceil(2);
    let mut codes = vec![0_u8; row_bytes * rows.len()];
    let mut factors = Vec::with_capacity(rows.len());
    for (row, output) in rows.iter().zip(codes.chunks_exact_mut(row_bytes)) {
        factors.push(quantize_bit4(row, output).expect("valid row"));
    }
    let expected = codes
        .chunks_exact(row_bytes)
        .zip(&factors)
        .map(|(row, &factor)| est_dot_bit4(&query, row, factor).expect("valid score"))
        .collect::<Vec<_>>();
    let mut actual = vec![f32::NAN; rows.len()];

    est_dot_bit4_batch(&query, &codes, &factors, &mut actual).expect("valid batch");

    assert_eq!(
        actual
            .iter()
            .map(|score| score.to_bits())
            .collect::<Vec<_>>(),
        expected
            .iter()
            .map(|score| score.to_bits())
            .collect::<Vec<_>>()
    );
}

#[test]
fn bit4_batch_estimator_rejects_shape_and_padding_errors() {
    let values = [0.25_f32, -0.5, 0.75];
    let query = prepare_bit4_query(&values, 0x4ba7_c004).expect("valid query");
    let mut codes = vec![0_u8; values.len().div_ceil(2)];
    let factor = quantize_bit4(&values, &mut codes).expect("valid row");

    assert_eq!(
        est_dot_bit4_batch(&query, &codes, &[factor], &mut []),
        Err(QuantError::OutputLength {
            expected: 1,
            actual: 0,
        })
    );
    assert_eq!(
        est_dot_bit4_batch(&query, &codes[..1], &[factor], &mut [f32::NAN]),
        Err(QuantError::CodeLength {
            expected: 2,
            actual: 1,
        })
    );
    codes[1] |= 0x0f;
    assert_eq!(
        est_dot_bit4_batch(&query, &codes, &[factor], &mut [f32::NAN]),
        Err(QuantError::NonZeroPadding {
            byte: codes[1],
            mask: 0x0f,
        })
    );
}

#[test]
fn bit4_rejects_non_finite_values_without_writing() {
    for (input, index) in [
        (vec![0.0, f32::NAN, 1.0], 1),
        (vec![f32::INFINITY, 0.0, 1.0], 0),
        (vec![0.0, 1.0, f32::NEG_INFINITY], 2),
    ] {
        let mut bit4 = vec![0x5a; input.len().div_ceil(2)];
        assert_eq!(
            quantize_bit4(&input, &mut bit4),
            Err(QuantError::NonFinite { index })
        );
        assert!(bit4.iter().all(|&byte| byte == 0x5a));
        assert_eq!(
            prepare_bit4_query(&input, 0),
            Err(QuantError::NonFinite { index })
        );
    }
}

#[test]
fn int8_rejects_non_finite_values_without_writing() {
    for (input, index) in [
        (vec![0.0, f32::NAN, 1.0], 1),
        (vec![f32::INFINITY, 0.0, 1.0], 0),
        (vec![0.0, 1.0, f32::NEG_INFINITY], 2),
    ] {
        let mut int8 = vec![0x2a_i8; input.len()];
        assert_eq!(
            quantize_int8(&input, &mut int8),
            Err(QuantError::NonFinite { index })
        );
        assert!(int8.iter().all(|code| *code == 0x2a));
        assert_eq!(
            prepare_int8_query(&input),
            Err(QuantError::NonFinite { index })
        );
    }
}

#[test]
fn signed_zero_rows_use_canonical_inner_positive_codes() {
    let input = [0.0, -0.0, 0.0, -0.0, 0.0];
    let mut bit4 = [u8::MAX; 3];
    let factors4 = quantize_bit4(&input, &mut bit4).expect("zero row");

    assert_eq!(bit4, [0x88, 0x88, 0x80]);
    assert_eq!(factors4.norm(), 0.0);
    assert_eq!(factors4.correction(), 0.0);
    assert_eq!(factors4.reconstruction_error_bound(), 0.0);

    let query4 = prepare_bit4_query(&input, 17).expect("zero query");
    assert_eq!(query4.len(), input.len());
    assert!(!query4.is_empty());
    assert_eq!(est_dot_bit4(&query4, &bit4, factors4), Ok(0.0));
}

#[test]
fn extreme_finite_rows_keep_usable_factors_and_reconstruction() {
    let bit4_input = [-1.682_141_7e38_f32; 5];

    let mut bit4 = [0_u8; 3];
    let factors4 = quantize_bit4(&bit4_input, &mut bit4).expect("finite extreme bit4 row");

    assert!(factors4.norm().is_finite());
    assert!(factors4.correction().is_finite());
    assert!(factors4.reconstruction_error_bound().is_finite());

    let mut reconstructed4 = [f32::NAN; 5];
    dequantize_bit4(&bit4, factors4, &mut reconstructed4).expect("extreme bit4 code");
    assert!(reconstructed4.iter().all(|value| value.is_finite()));
}

#[test]
fn partial_byte_padding_is_canonical_and_checked() {
    let input = fixture_f32(include_str!("../../tests/fixtures/quant/narrow_input.txt"));
    let mut bit4 = vec![0_u8; input.len().div_ceil(2)];
    let factors4 = quantize_bit4(&input, &mut bit4).expect("valid row");
    let query4 = prepare_bit4_query(&input, 23).expect("valid query");

    *bit4.last_mut().expect("partial byte exists") |= 0x01;
    assert_eq!(
        est_dot_bit4(&query4, &bit4, factors4),
        Err(QuantError::NonZeroPadding {
            byte: 0x81,
            mask: 0x0f,
        })
    );
}

#[test]
fn bit4_shape_errors_are_typed() {
    assert_eq!(quantize_bit4(&[], &mut []), Err(QuantError::EmptyVector));
    assert_eq!(prepare_bit4_query(&[], 0), Err(QuantError::EmptyVector));

    let input = [1.0_f32; 5];
    assert_eq!(
        quantize_bit4(&input, &mut [0_u8; 2]),
        Err(QuantError::OutputLength {
            expected: 3,
            actual: 2,
        })
    );

    let mut bit4 = [0_u8; 3];
    let factors4 = quantize_bit4(&input, &mut bit4).expect("valid row");
    let query4 = prepare_bit4_query(&input, 31).expect("valid query");
    assert_eq!(
        est_dot_bit4(&query4, &bit4[..2], factors4),
        Err(QuantError::CodeLength {
            expected: 3,
            actual: 2,
        })
    );
}

#[test]
fn int8_shape_errors_are_typed() {
    assert_eq!(quantize_int8(&[], &mut []), Err(QuantError::EmptyVector));
    assert_eq!(prepare_int8_query(&[]), Err(QuantError::EmptyVector));

    let input = [1.0_f32; 9];
    assert_eq!(
        quantize_int8(&input, &mut [0_i8; 8]),
        Err(QuantError::OutputLength {
            expected: 9,
            actual: 8,
        })
    );
    let mut int8_codes = [0_i8; 9];
    let (scale, offset) = quantize_int8(&input, &mut int8_codes).expect("int8 row");
    let int8_query = prepare_int8_query(&input).expect("int8 query");
    assert_eq!(int8_query.len(), input.len());
    assert!(!int8_query.is_empty());
    assert_eq!(
        dot_int8_query(
            &int8_query,
            Int8Vec {
                codes: &int8_codes[..8],
                scale,
                offset,
            },
        ),
        Err(QuantError::CodeLength {
            expected: 9,
            actual: 8,
        })
    );
    assert_eq!(
        dequantize_int8(
            Int8Vec {
                codes: &int8_codes,
                scale,
                offset,
            },
            &mut [0.0_f32; 8],
        ),
        Err(QuantError::OutputLength {
            expected: 9,
            actual: 8,
        })
    );
}

#[test]
fn surviving_quantizers_reject_dimensions_beyond_i8_kernel_limit() {
    let input = vec![0.0_f32; MAX_DOT_I8_DIMENSION + 1];
    let expected = QuantError::DimensionTooLarge {
        actual: input.len(),
        maximum: MAX_DOT_I8_DIMENSION,
    };
    let mut bit4 = vec![0_u8; input.len().div_ceil(2)];
    let mut int8 = vec![0_i8; input.len()];

    assert_eq!(quantize_int8(&input, &mut int8), Err(expected.clone()));
    assert_eq!(quantize_bit4(&input, &mut bit4), Err(expected.clone()));
    assert_eq!(prepare_int8_query(&input), Err(expected.clone()));
    assert_eq!(prepare_bit4_query(&input, 0), Err(expected));
}

#[test]
fn quant_errors_have_actionable_messages() {
    let errors = [
        QuantError::EmptyVector,
        QuantError::DimensionTooLarge {
            actual: 9,
            maximum: 8,
        },
        QuantError::NonFinite { index: 3 },
        QuantError::OutputLength {
            expected: 2,
            actual: 1,
        },
        QuantError::CodeLength {
            expected: 3,
            actual: 2,
        },
        QuantError::NonZeroPadding {
            byte: 0x81,
            mask: 0x0f,
        },
    ];

    for error in errors {
        assert!(!error.to_string().is_empty());
    }
}

#[test]
fn prop_int8_rank_preservation() {
    const DIMENSION: usize = 768;
    const CLUSTERS: usize = 12;
    const ROWS_PER_CLUSTER: usize = 8;
    const TRIALS: usize = 100;
    let mut random = crate::test_support::seeded_rng("quant::prop_int8_rank_preservation");
    let mut int8_top1_matches = 0_usize;

    for trial in 0..TRIALS {
        let mut centers = (0..CLUSTERS)
            .map(|_| {
                let mut center = (0..DIMENSION)
                    .map(|_| random.random_range(-1.0_f32..=1.0_f32))
                    .collect::<Vec<_>>();
                normalize(&mut center);
                center
            })
            .collect::<Vec<_>>();
        let query = centers.swap_remove(trial % CLUSTERS);
        centers.push(query.clone());

        let mut rows = Vec::with_capacity(CLUSTERS * ROWS_PER_CLUSTER);
        for center in &centers {
            rows.push(center.clone());
            for _ in 1..ROWS_PER_CLUSTER {
                let mut row = center
                    .iter()
                    .map(|&value| value + random.random_range(-0.01_f32..=0.01_f32))
                    .collect::<Vec<_>>();
                normalize(&mut row);
                rows.push(row);
            }
        }

        let exact_scores = rows
            .iter()
            .map(|row| dot_reference(row, &query))
            .collect::<Vec<_>>();
        let exact_top10 = rank_descending(&exact_scores, 10);

        let int8_query = prepare_int8_query(&query).expect("finite query");
        let mut int8_scores = Vec::with_capacity(rows.len());
        for row in &rows {
            let mut int8_codes = vec![0_i8; DIMENSION];
            let (scale, offset) = quantize_int8(row, &mut int8_codes).expect("finite row");
            int8_scores.push(f64::from(
                dot_int8_query(
                    &int8_query,
                    Int8Vec {
                        codes: &int8_codes,
                        scale,
                        offset,
                    },
                )
                .expect("matching int8 row"),
            ));
        }

        let int8_top1 = rank_descending(&int8_scores, 1);
        if int8_top1.first() == exact_top10.first() {
            int8_top1_matches += 1;
        }
    }

    assert!(
        int8_top1_matches * 100 >= TRIALS * 99,
        "int8 preserved top-1 in {int8_top1_matches}/{TRIALS} trials"
    );
}

#[test]
fn rescore_returns_exact_topk_when_oversample_covers() {
    const DIMENSION: usize = 17;
    const ROW_COUNT: usize = 37;
    const K: usize = 5;
    const OVERSAMPLE: usize = 8;
    const COARSE_BYTES_PER_ROW: usize = 13;
    let mut random =
        crate::test_support::seeded_rng("quant::rescore_returns_exact_topk_when_oversample_covers");
    let query = (0..DIMENSION)
        .map(|_| random.random_range(-1.0_f32..=1.0_f32))
        .collect::<Vec<_>>();
    let rows = (0..ROW_COUNT)
        .flat_map(|_| {
            (0..DIMENSION)
                .map(|_| random.random_range(-1.0_f32..=1.0_f32))
                .collect::<Vec<_>>()
        })
        .collect::<Vec<_>>();
    let coarse_scores = (0..ROW_COUNT)
        .map(|index| -(index as f32))
        .collect::<Vec<_>>();
    let exact_scores = rows
        .chunks_exact(DIMENSION)
        .map(|row| dot_reference(row, &query))
        .collect::<Vec<_>>();
    let expected = rank_descending(&exact_scores, K);

    let pool = RescorePool::dense(&coarse_scores, OVERSAMPLE, COARSE_BYTES_PER_ROW);
    let result = rescore_top_k(&query, &rows, DIMENSION, pool, K).expect("valid two-stage search");
    let actual = result
        .hits
        .iter()
        .map(|hit| hit.row_index)
        .collect::<Vec<_>>();

    assert_eq!(actual, expected);
    assert_eq!(result.bytes.coarse, ROW_COUNT * COARSE_BYTES_PER_ROW);
    assert_eq!(result.bytes.rescore, ROW_COUNT * DIMENSION * 4);
    assert_eq!(
        result.bytes.total(),
        result.bytes.coarse + result.bytes.rescore
    );
}

#[test]
fn widened_row_prefetch_preserves_rescore_top_k_scores() {
    const DIMENSION: usize = 129;
    const ROW_COUNT: usize = 12;
    const K: usize = 8;
    let query = (0..DIMENSION)
        .map(|index| (index as f32).mul_add(0.03125, -2.0))
        .collect::<Vec<_>>();
    let rows = (0..ROW_COUNT)
        .flat_map(|row| {
            (0..DIMENSION)
                .map(move |column| ((row * DIMENSION + column) as f32).mul_add(0.015625, -11.0))
        })
        .collect::<Vec<_>>();
    let row_indices = [8_u32, 1, 10, 3, 6, 11, 0, 9, 4, 7, 2, 5];
    let coarse_scores = [
        12.0_f32, 11.0, 10.0, 9.0, 8.0, 7.0, 6.0, 5.0, 4.0, 3.0, 2.0, 1.0,
    ];
    let pool = RescorePool::retained(
        &row_indices,
        &coarse_scores,
        RescoreMetric::InnerProduct,
        ROW_COUNT,
        DIMENSION * std::mem::size_of::<f32>(),
    );

    let without_prefetch = rescore_top_k(&query, &rows, DIMENSION, pool.with_prefetch(false), K)
        .expect("rescore without prefetch is valid");
    let with_prefetch = rescore_top_k(&query, &rows, DIMENSION, pool.with_prefetch(true), K)
        .expect("rescore with prefetch is valid");
    let without_prefetch_scores = without_prefetch
        .hits
        .iter()
        .map(|hit| (hit.row_index, hit.score.to_bits()))
        .collect::<Vec<_>>();
    let with_prefetch_scores = with_prefetch
        .hits
        .iter()
        .map(|hit| (hit.row_index, hit.score.to_bits()))
        .collect::<Vec<_>>();

    assert_eq!(with_prefetch_scores, without_prefetch_scores);
}

#[test]
fn retained_pool_rescore_uses_exact_squared_l2_and_original_row_ids() {
    let query = [0.0_f32, 0.0];
    let rows = [100.0_f32, 0.0, 2.0, 0.0, 0.0, 2.0];
    let row_indices = [0_u32, 2, 1];
    let coarse_scores = [10.0_f32, 9.0, 8.0];
    let pool = RescorePool::retained(
        &row_indices,
        &coarse_scores,
        RescoreMetric::SquaredL2,
        17,
        76,
    );

    let result = rescore_top_k(&query, &rows, 2, pool, 2).expect("retained pool is valid");

    assert_eq!(
        result
            .hits
            .iter()
            .map(|hit| hit.row_index)
            .collect::<Vec<_>>(),
        vec![1, 2]
    );
    assert_eq!(result.candidates_rescored, 3);
    assert_eq!(result.bytes.coarse, 17 * 76);
    assert_eq!(result.bytes.rescore, 3 * 2 * 4);
}

#[test]
fn rescore_cancellation_checkpoint_fires_before_row_sixty_four_is_scored() {
    const ROW_COUNT: usize = 65;
    let query = [0.0_f32];
    let mut rows = vec![0.0_f32; ROW_COUNT];
    rows[64] = f32::NAN;
    let row_indices = (0_u32..ROW_COUNT as u32).collect::<Vec<_>>();
    let coarse_scores = vec![0.0_f32; ROW_COUNT];
    let pool = RescorePool::retained(&row_indices, &coarse_scores, RescoreMetric::SquaredL2, 0, 0);
    let mut checkpoints = Vec::new();

    let error = rescore_top_k_with_check(
        &query,
        &rows,
        1,
        pool,
        ROW_COUNT,
        |row_index, is_checkpoint| {
            if is_checkpoint {
                checkpoints.push(row_index);
            }
            if row_index == 64 {
                Err("cancelled")
            } else {
                Ok(())
            }
        },
    )
    .expect_err("the row-64 checkpoint must stop exact scoring");

    assert_eq!(error, RescoreCheckError::Check("cancelled"));
    assert_eq!(checkpoints, vec![0, 64]);
}

#[test]
fn retained_pool_rejects_out_of_range_row_without_panicking() {
    let row_indices = [u32::MAX];
    let coarse_scores = [1.0_f32];
    let pool = RescorePool::retained(&row_indices, &coarse_scores, RescoreMetric::SquaredL2, 1, 1);

    assert_eq!(
        rescore_top_k(&[0.0], &[0.0], 1, pool, 1),
        Err(RescoreError::CandidateRowOutOfRange {
            position: 0,
            row_index: u32::MAX as usize,
            row_count: 1,
        })
    );
}

#[test]
fn rescore_shape_and_counter_errors_are_typed() {
    assert_eq!(
        rescore_top_k(&[], &[], 0, RescorePool::dense(&[], 1, 1), 1),
        Err(RescoreError::ZeroDimension)
    );
    assert_eq!(
        rescore_top_k(&[1.0], &[1.0, 2.0], 2, RescorePool::dense(&[1.0], 1, 1), 1,),
        Err(RescoreError::QueryDimension {
            expected: 2,
            actual: 1,
        })
    );
    assert_eq!(
        rescore_top_k(&[1.0, 2.0], &[1.0], 2, RescorePool::dense(&[], 1, 1), 1,),
        Err(RescoreError::RowDataLength {
            dimension: 2,
            actual: 1,
        })
    );
    assert_eq!(
        rescore_top_k(&[1.0], &[1.0], 1, RescorePool::dense(&[], 1, 1), 1,),
        Err(RescoreError::CoarseScoreCount {
            expected: 1,
            actual: 0,
        })
    );
    assert_eq!(
        rescore_top_k(&[1.0], &[1.0], 1, RescorePool::dense(&[1.0], 1, 1), 0,),
        Err(RescoreError::ZeroK)
    );
    assert_eq!(
        rescore_top_k(&[1.0], &[1.0], 1, RescorePool::dense(&[1.0], 0, 1), 1,),
        Err(RescoreError::ZeroOversample)
    );
    assert_eq!(
        rescore_top_k(&[1.0], &[1.0], 1, RescorePool::dense(&[f32::NAN], 1, 1), 1,),
        Err(RescoreError::NonFiniteCoarseScore { index: 0 })
    );
    assert_eq!(
        rescore_top_k(
            &[1.0],
            &[1.0],
            1,
            RescorePool::dense(&[1.0], 2, 1),
            usize::MAX,
        ),
        Err(RescoreError::ArithmeticOverflow)
    );
    assert_eq!(
        rescore_top_k(
            &[1.0],
            &[1.0, 2.0],
            1,
            RescorePool::dense(&[1.0, 2.0], 1, usize::MAX),
            1,
        ),
        Err(RescoreError::ArithmeticOverflow)
    );

    let errors = [
        RescoreError::ZeroDimension,
        RescoreError::QueryDimension {
            expected: 2,
            actual: 1,
        },
        RescoreError::RowDataLength {
            dimension: 2,
            actual: 1,
        },
        RescoreError::CoarseScoreCount {
            expected: 2,
            actual: 1,
        },
        RescoreError::CandidateRowCount {
            expected: 2,
            actual: 1,
        },
        RescoreError::CandidateRowOutOfRange {
            position: 0,
            row_index: 2,
            row_count: 1,
        },
        RescoreError::ZeroK,
        RescoreError::ZeroOversample,
        RescoreError::NonFiniteCoarseScore { index: 0 },
        RescoreError::NonFiniteExactScore { row_index: 0 },
        RescoreError::InsufficientCandidates {
            k: 2,
            candidates: 1,
        },
        RescoreError::ArithmeticOverflow,
    ];
    for error in errors {
        assert!(!error.to_string().is_empty());
    }
}

#[test]
fn prop_bit4_estimator_unbiased_in_expectation() {
    const DIMENSION: usize = 768;
    const QUERY_COUNT: usize = 512;
    let mut random =
        crate::test_support::seeded_rng("quant::prop_bit4_estimator_unbiased_in_expectation");
    let row = (0..DIMENSION)
        .map(|_| random.random_range(-1.0_f32..=1.0_f32))
        .collect::<Vec<_>>();
    let mut codes = vec![0_u8; DIMENSION / 2];
    let factors = quantize_bit4(&row, &mut codes).expect("finite row");
    let mut errors = Vec::with_capacity(QUERY_COUNT);

    for _ in 0..QUERY_COUNT {
        let query_values = (0..DIMENSION)
            .map(|_| random.random_range(-1.0_f32..=1.0_f32))
            .collect::<Vec<_>>();
        let query = prepare_bit4_query(&query_values, random.random()).expect("finite query");
        let estimate = est_dot_bit4(&query, &codes, factors).expect("matching code");
        let exact = dot_reference(&row, &query_values);
        errors.push(f64::from(estimate) - exact);
    }

    assert_mean_is_statistically_zero(&errors);
}

fn dot_reference(left: &[f32], right: &[f32]) -> f64 {
    left.iter()
        .zip(right)
        .map(|(&left, &right)| f64::from(left) * f64::from(right))
        .sum()
}

fn rank_descending(scores: &[f64], k: usize) -> Vec<usize> {
    let mut ranked = scores.iter().copied().enumerate().collect::<Vec<_>>();
    ranked.sort_unstable_by(|left, right| {
        right
            .1
            .total_cmp(&left.1)
            .then_with(|| left.0.cmp(&right.0))
    });
    ranked.into_iter().take(k).map(|(index, _)| index).collect()
}

fn assert_mean_is_statistically_zero(errors: &[f64]) {
    let count = errors.len() as f64;
    let mean = errors.iter().sum::<f64>() / count;
    let variance = errors
        .iter()
        .map(|error| {
            let centered = error - mean;
            centered * centered
        })
        .sum::<f64>()
        / count;
    let standard_error = (variance / count).sqrt();
    let rounding = 1.0e-5_f64;
    assert!(
        mean.abs() <= 6.0 * standard_error + rounding,
        "mean signed error {mean:?} exceeded six standard errors {standard_error:?}"
    );
}

fn correlated_unit_pair(seed: u64, name: &str) -> (Vec<f32>, Vec<f32>) {
    const DIMENSION: usize = 768;
    let mut random = crate::test_support::seeded_rng_from(name, &seed.to_string());
    let mut row = (0..DIMENSION)
        .map(|_| random.random_range(-1.0_f32..=1.0_f32))
        .collect::<Vec<_>>();
    normalize(&mut row);
    let mut orthogonal = (0..DIMENSION)
        .map(|_| random.random_range(-1.0_f32..=1.0_f32))
        .collect::<Vec<_>>();
    let projection = dot_reference(&row, &orthogonal) as f32;
    for (value, &axis) in orthogonal.iter_mut().zip(&row) {
        *value -= projection * axis;
    }
    normalize(&mut orthogonal);
    let perpendicular_weight = 0.75_f32.sqrt();
    let query = row
        .iter()
        .zip(&orthogonal)
        .map(|(&axis, &perpendicular)| 0.5 * axis + perpendicular_weight * perpendicular)
        .collect();
    (row, query)
}

fn normalize(values: &mut [f32]) {
    let norm = values
        .iter()
        .map(|&value| {
            let value = f64::from(value);
            value * value
        })
        .sum::<f64>()
        .sqrt();
    for value in values {
        *value = (f64::from(*value) / norm) as f32;
    }
}

fn dot_magnitude(left: &[f32], right: &[f32]) -> f64 {
    left.iter()
        .zip(right)
        .map(|(&left, &right)| f64::from(left).abs() * f64::from(right).abs())
        .sum()
}

fn assert_estimate_within_paper_bound(bits: i32, row: &[f32], query: &[f32], estimate: f32) {
    let exact = dot_reference(row, query);
    let row_norm = dot_reference(row, row).sqrt();
    let query_norm = dot_reference(query, query).sqrt();
    let paper_bound =
        5.75 * 2.0_f64.powi(-bits) / (row.len() as f64).sqrt() * row_norm * query_norm;
    let magnitude = dot_magnitude(row, query);
    let backward_rounding = 1.0e-5_f64 * magnitude.max(1.0);
    let error = (f64::from(estimate) - exact).abs();
    assert!(
        error <= paper_bound + backward_rounding,
        "estimate error {error:?} exceeded paper bound {paper_bound:?} plus backward rounding {backward_rounding:?}; exact={exact:?} estimate={estimate:?} magnitude={magnitude:?}"
    );
}

fn finite_vectors() -> impl Strategy<Value = Vec<f32>> {
    prop_oneof![
        vec(
            any::<f32>().prop_filter("finite coordinate", |value| value.is_finite()),
            1..=96
        ),
        (
            1_usize..=96,
            any::<f32>().prop_filter("finite constant", |value| value.is_finite()),
        )
            .prop_map(|(length, value)| vec![value; length]),
        Just(vec![0.0, -0.0, 0.0, -0.0, 0.0]),
    ]
}

proptest! {
    #![proptest_config(ProptestConfig {
        rng_seed: RngSeed::Fixed(0x5eed_04b2_04b4),
        ..ProptestConfig::default()
    })]
    #[test]
    fn prop_int8_roundtrip_error_bounded(input in finite_vectors()) {
        let mut codes = vec![0_i8; input.len()];
        let (scale, offset) = quantize_int8(&input, &mut codes).expect("finite sized input");
        let encoded = Int8Vec {
            codes: &codes,
            scale,
            offset,
        };
        let mut reconstructed = vec![f32::NAN; input.len()];
        dequantize_int8(encoded, &mut reconstructed).expect("matching output");

        let minimum = input.iter().copied().fold(f32::INFINITY, f32::min);
        let maximum = input
            .iter()
            .copied()
            .fold(f32::NEG_INFINITY, f32::max);
        let bound = (f64::from(maximum) - f64::from(minimum)) / 254.0;
        for (&actual, &approximation) in input.iter().zip(&reconstructed) {
            let error = (f64::from(actual) - f64::from(approximation)).abs();
            prop_assert!(
                error <= bound,
                "value={actual:?} approximation={approximation:?} error={error:?} bound={bound:?} scale={scale:?} offset={offset:?}"
            );
        }
    }

    #[test]
    fn prop_int8_dot_estimate_close(seed in any::<u64>()) {
        let (row, query_values) = correlated_unit_pair(seed, "quant::prop_int8_dot_estimate_close");
        let mut codes = vec![0_i8; row.len()];
        let (scale, offset) = quantize_int8(&row, &mut codes).expect("finite row");
        let query = prepare_int8_query(&query_values).expect("finite query");
        let estimate = dot_int8_query(
            &query,
            Int8Vec {
                codes: &codes,
                scale,
                offset,
            },
        )
        .expect("matching code");

        let exact = dot_reference(&row, &query_values);
        let magnitude = dot_magnitude(&row, &query_values);
        let error = (f64::from(estimate) - exact).abs();
        prop_assert!(
            error <= 0.02 * magnitude.max(1.0e-30),
            "estimate={estimate:?} exact={exact:?} error={error:?} magnitude={magnitude:?}"
        );
    }

    #[test]
    fn prop_bit4_roundtrip_error_bounded(input in finite_vectors()) {
        let mut codes = vec![0_u8; input.len().div_ceil(2)];
        let factors = quantize_bit4(&input, &mut codes).expect("finite sized input");
        prop_assert!(factors.norm().is_finite());
        prop_assert!(factors.correction().is_finite());
        prop_assert!(factors.reconstruction_error_bound().is_finite());
        let mut reconstructed = vec![f32::NAN; input.len()];
        dequantize_bit4(&codes, factors, &mut reconstructed).expect("matching code");
        prop_assert!(reconstructed.iter().all(|value| value.is_finite()));

        let error = input
            .iter()
            .zip(&reconstructed)
            .map(|(&actual, &approximation)| {
                let difference = f64::from(actual) - f64::from(approximation);
                difference * difference
            })
            .sum::<f64>()
            .sqrt();
        let rounding = 1.0e-5_f64 * factors.norm().max(1.0);
        prop_assert!(
            error <= factors.reconstruction_error_bound() + rounding,
            "error={error:?} bound={:?} rounding={rounding:?}",
            factors.reconstruction_error_bound(),
        );
    }
    #[test]
    fn prop_bit4_dot_estimate_close(seed in any::<u64>()) {
        let (row, query_values) = correlated_unit_pair(seed, "quant::prop_bit4_dot_estimate_close");
        let mut codes = vec![0_u8; row.len().div_ceil(2)];
        let factors = quantize_bit4(&row, &mut codes).expect("finite row");
        let query = prepare_bit4_query(&query_values, seed ^ 0x4b17_4b17)
            .expect("finite query");
        let estimate = est_dot_bit4(&query, &codes, factors).expect("matching code");

        assert_estimate_within_paper_bound(4, &row, &query_values, estimate);
    }
}
