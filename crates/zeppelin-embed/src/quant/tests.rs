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
    QuantError, QuantScheme, dequantize_bit2, dequantize_bit4, est_dot_bit2, est_dot_bit4,
    prepare_bit2_query, prepare_bit4_query, quantize_bit2, quantize_bit4,
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
fn persisted_scheme_ids_are_append_only() {
    assert_eq!(QuantScheme::F32.id(), 0);
    assert_eq!(QuantScheme::F16.id(), 1);
    assert_eq!(QuantScheme::Int8.id(), 2);
    assert_eq!(QuantScheme::Bit1.id(), 3);
    assert_eq!(QuantScheme::Bit4.id(), 4);
    assert_eq!(QuantScheme::Bit2.id(), 5);
    for id in 0..=5 {
        assert_eq!(QuantScheme::from_id(id).map(QuantScheme::id), Some(id));
    }
    assert_eq!(QuantScheme::from_id(6), None);
    assert_eq!(QuantScheme::from_id(u8::MAX), None);
}

#[test]
fn narrow_row_sizes_at_dimension_768_are_pinned() {
    let input = vec![0.0_f32; 768];
    let mut bit2 = vec![0_u8; input.len().div_ceil(4)];
    let mut bit4 = vec![0_u8; input.len().div_ceil(2)];

    quantize_bit2(&input, &mut bit2).expect("valid bit2 row");
    quantize_bit4(&input, &mut bit4).expect("valid bit4 row");

    assert_eq!(bit2.len(), 192);
    assert_eq!(bit4.len(), 384);
}

#[test]
fn bit2_golden_fixture_is_stable() {
    let input = fixture_f32(include_str!("../../tests/fixtures/quant/narrow_input.txt"));
    let expected = fixture_hex(include_str!("../../tests/fixtures/quant/bit2_code.hex"));
    let mut actual = vec![u8::MAX; expected.len()];

    let factors = quantize_bit2(&input, &mut actual).expect("fixture is finite and sized");

    assert_eq!(actual, expected);
    assert_eq!(factors.norm(), 1.0);
    assert_eq!(factors.correction(), f64::from(2.0_f32 / 3.0));
    let mut reconstructed = vec![0.0; input.len()];
    dequantize_bit2(&actual, factors, &mut reconstructed).expect("golden code decodes");
    let mut roundtrip = vec![0_u8; actual.len()];
    quantize_bit2(&reconstructed, &mut roundtrip).expect("reconstruction re-encodes");
    assert_eq!(roundtrip, expected);
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
fn bit2_native_estimator_scores_aligned_vector() {
    let input = fixture_f32(include_str!("../../tests/fixtures/quant/narrow_input.txt"));
    let mut codes = vec![0_u8; input.len().div_ceil(4)];
    let factors = quantize_bit2(&input, &mut codes).expect("valid row");
    let query = prepare_bit2_query(&input, 7).expect("valid query");

    let estimate = est_dot_bit2(&query, &codes, factors).expect("matching code");

    assert!((estimate - 1.0).abs() <= f32::EPSILON);
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
fn narrow_quantizers_reject_non_finite_values_without_writing() {
    for (input, index) in [
        (vec![0.0, f32::NAN, 1.0], 1),
        (vec![f32::INFINITY, 0.0, 1.0], 0),
        (vec![0.0, 1.0, f32::NEG_INFINITY], 2),
    ] {
        let mut bit2 = vec![0xa5; input.len().div_ceil(4)];
        let mut bit4 = vec![0x5a; input.len().div_ceil(2)];
        assert_eq!(
            quantize_bit2(&input, &mut bit2),
            Err(QuantError::NonFinite { index })
        );
        assert_eq!(
            quantize_bit4(&input, &mut bit4),
            Err(QuantError::NonFinite { index })
        );
        assert!(bit2.iter().all(|&byte| byte == 0xa5));
        assert!(bit4.iter().all(|&byte| byte == 0x5a));
        assert_eq!(
            prepare_bit2_query(&input, 0),
            Err(QuantError::NonFinite { index })
        );
        assert_eq!(
            prepare_bit4_query(&input, 0),
            Err(QuantError::NonFinite { index })
        );
    }
}

#[test]
fn signed_zero_rows_use_canonical_inner_positive_codes() {
    let input = [0.0, -0.0, 0.0, -0.0, 0.0];
    let mut bit2 = [u8::MAX; 2];
    let mut bit4 = [u8::MAX; 3];
    let factors2 = quantize_bit2(&input, &mut bit2).expect("zero row");
    let factors4 = quantize_bit4(&input, &mut bit4).expect("zero row");

    assert_eq!(bit2, [0xaa, 0x80]);
    assert_eq!(bit4, [0x88, 0x88, 0x80]);
    assert_eq!(factors2.norm(), 0.0);
    assert_eq!(factors2.correction(), 0.0);
    assert_eq!(factors2.reconstruction_error_bound(), 0.0);
    assert_eq!(factors4.norm(), 0.0);
    assert_eq!(factors4.correction(), 0.0);
    assert_eq!(factors4.reconstruction_error_bound(), 0.0);

    let query2 = prepare_bit2_query(&input, 13).expect("zero query");
    let query4 = prepare_bit4_query(&input, 17).expect("zero query");
    assert_eq!(query2.len(), input.len());
    assert_eq!(query4.len(), input.len());
    assert!(!query2.is_empty());
    assert!(!query4.is_empty());
    assert_eq!(est_dot_bit2(&query2, &bit2, factors2), Ok(0.0));
    assert_eq!(est_dot_bit4(&query4, &bit4, factors4), Ok(0.0));
}

#[test]
fn extreme_finite_rows_keep_usable_factors_and_reconstruction() {
    let bit2_input = [-2.801_187e38_f32];
    let bit4_input = [-1.682_141_7e38_f32; 5];

    let mut bit2 = [0_u8; 1];
    let mut bit4 = [0_u8; 3];
    let factors2 = quantize_bit2(&bit2_input, &mut bit2).expect("finite extreme bit2 row");
    let factors4 = quantize_bit4(&bit4_input, &mut bit4).expect("finite extreme bit4 row");

    assert!(factors2.norm().is_finite());
    assert!(factors2.correction().is_finite());
    assert!(factors2.reconstruction_error_bound().is_finite());
    assert!(factors4.norm().is_finite());
    assert!(factors4.correction().is_finite());
    assert!(factors4.reconstruction_error_bound().is_finite());

    let mut reconstructed2 = [f32::NAN; 1];
    let mut reconstructed4 = [f32::NAN; 5];
    dequantize_bit2(&bit2, factors2, &mut reconstructed2).expect("extreme bit2 code");
    dequantize_bit4(&bit4, factors4, &mut reconstructed4).expect("extreme bit4 code");
    assert!(reconstructed2.iter().all(|value| value.is_finite()));
    assert!(reconstructed4.iter().all(|value| value.is_finite()));
}

#[test]
fn partial_byte_padding_is_canonical_and_checked() {
    let input = fixture_f32(include_str!("../../tests/fixtures/quant/narrow_input.txt"));
    let mut bit2 = vec![0_u8; input.len().div_ceil(4)];
    let mut bit4 = vec![0_u8; input.len().div_ceil(2)];
    let factors2 = quantize_bit2(&input, &mut bit2).expect("valid row");
    let factors4 = quantize_bit4(&input, &mut bit4).expect("valid row");
    let query2 = prepare_bit2_query(&input, 19).expect("valid query");
    let query4 = prepare_bit4_query(&input, 23).expect("valid query");

    *bit2.last_mut().expect("partial byte exists") |= 0x01;
    *bit4.last_mut().expect("partial byte exists") |= 0x01;
    assert_eq!(
        est_dot_bit2(&query2, &bit2, factors2),
        Err(QuantError::NonZeroPadding {
            byte: 0x81,
            mask: 0x3f,
        })
    );
    assert_eq!(
        est_dot_bit4(&query4, &bit4, factors4),
        Err(QuantError::NonZeroPadding {
            byte: 0x81,
            mask: 0x0f,
        })
    );
}

#[test]
fn narrow_quantizer_shape_errors_are_typed() {
    assert_eq!(quantize_bit2(&[], &mut []), Err(QuantError::EmptyVector));
    assert_eq!(quantize_bit4(&[], &mut []), Err(QuantError::EmptyVector));
    assert_eq!(prepare_bit2_query(&[], 0), Err(QuantError::EmptyVector));
    assert_eq!(prepare_bit4_query(&[], 0), Err(QuantError::EmptyVector));

    let input = [1.0_f32; 5];
    assert_eq!(
        quantize_bit2(&input, &mut [0_u8; 1]),
        Err(QuantError::OutputLength {
            expected: 2,
            actual: 1,
        })
    );
    assert_eq!(
        quantize_bit4(&input, &mut [0_u8; 2]),
        Err(QuantError::OutputLength {
            expected: 3,
            actual: 2,
        })
    );

    let mut bit2 = [0_u8; 2];
    let mut bit4 = [0_u8; 3];
    let factors2 = quantize_bit2(&input, &mut bit2).expect("valid row");
    let factors4 = quantize_bit4(&input, &mut bit4).expect("valid row");
    let query2 = prepare_bit2_query(&input, 29).expect("valid query");
    let query4 = prepare_bit4_query(&input, 31).expect("valid query");
    assert_eq!(
        est_dot_bit2(&query2, &bit2[..1], factors2),
        Err(QuantError::CodeLength {
            expected: 2,
            actual: 1,
        })
    );
    assert_eq!(
        est_dot_bit4(&query4, &bit4[..2], factors4),
        Err(QuantError::CodeLength {
            expected: 3,
            actual: 2,
        })
    );
}

#[test]
fn narrow_quantizers_reject_dimensions_beyond_i8_kernel_limit() {
    let input = vec![0.0_f32; MAX_DOT_I8_DIMENSION + 1];
    let expected = QuantError::DimensionTooLarge {
        actual: input.len(),
        maximum: MAX_DOT_I8_DIMENSION,
    };
    let mut bit2 = vec![0_u8; input.len().div_ceil(4)];
    let mut bit4 = vec![0_u8; input.len().div_ceil(2)];

    assert_eq!(quantize_bit2(&input, &mut bit2), Err(expected.clone()));
    assert_eq!(quantize_bit4(&input, &mut bit4), Err(expected.clone()));
    assert_eq!(prepare_bit2_query(&input, 0), Err(expected.clone()));
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
fn prop_bit2_estimator_unbiased_in_expectation() {
    const DIMENSION: usize = 768;
    const QUERY_COUNT: usize = 512;
    let mut random =
        crate::test_support::seeded_rng("quant::prop_bit2_estimator_unbiased_in_expectation");
    let row = (0..DIMENSION)
        .map(|_| random.random_range(-1.0_f32..=1.0_f32))
        .collect::<Vec<_>>();
    let mut codes = vec![0_u8; DIMENSION / 4];
    let factors = quantize_bit2(&row, &mut codes).expect("finite row");
    let mut errors = Vec::with_capacity(QUERY_COUNT);

    for _ in 0..QUERY_COUNT {
        let query_values = (0..DIMENSION)
            .map(|_| random.random_range(-1.0_f32..=1.0_f32))
            .collect::<Vec<_>>();
        let query = prepare_bit2_query(&query_values, random.random()).expect("finite query");
        let estimate = est_dot_bit2(&query, &codes, factors).expect("matching code");
        let exact = dot_reference(&row, &query_values);
        errors.push(f64::from(estimate) - exact);
    }

    assert_mean_is_statistically_zero(&errors);
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
    fn prop_bit2_roundtrip_error_bounded(input in finite_vectors()) {
        let mut codes = vec![0_u8; input.len().div_ceil(4)];
        let factors = quantize_bit2(&input, &mut codes).expect("finite sized input");
        prop_assert!(factors.norm().is_finite());
        prop_assert!(factors.correction().is_finite());
        prop_assert!(factors.reconstruction_error_bound().is_finite());
        let mut reconstructed = vec![f32::NAN; input.len()];
        dequantize_bit2(&codes, factors, &mut reconstructed).expect("matching code");
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
    fn prop_bit2_dot_estimate_close(seed in any::<u64>()) {
        let (row, query_values) = correlated_unit_pair(seed, "quant::prop_bit2_dot_estimate_close");
        let mut codes = vec![0_u8; row.len().div_ceil(4)];
        let factors = quantize_bit2(&row, &mut codes).expect("finite row");
        let query = prepare_bit2_query(&query_values, seed ^ 0x2b17_2b17)
            .expect("finite query");
        let estimate = est_dot_bit2(&query, &codes, factors).expect("matching code");

        assert_estimate_within_paper_bound(2, &row, &query_values, estimate);
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
