#[allow(
    clippy::expect_used,
    clippy::indexing_slicing,
    clippy::panic,
    clippy::unwrap_used
)]
mod kernels {
    use std::process::Command;
    use std::time::Instant;

    use proptest::collection::vec;
    use proptest::prelude::*;
    use proptest::test_runner::RngSeed;
    use zeppelin_embed::kernels::{
        InstructionTier, KERNEL_KNOB_SPACE, KernelArm, KernelInitError, KernelVariant,
        MAX_DOT_I8_DIMENSION, detected_features, dot_bit2, dot_bit2_batch, dot_bit4,
        dot_bit4_batch, dot_f16, dot_f32, dot_i8, dot_i8_batch, hamming_u1, hamming_u1_batch,
        initialize, is_arm_supported, selected_arm,
    };
    use zeppelin_embed::quant::quantize_bit4;

    fn vector_pair_i8() -> impl Strategy<Value = (Vec<i8>, Vec<i8>)> {
        (1_usize..=4_096).prop_flat_map(|len| (vec(any::<i8>(), len), vec(any::<i8>(), len)))
    }

    fn vector_pair_u8() -> impl Strategy<Value = (Vec<u8>, Vec<u8>)> {
        (1_usize..=4_096).prop_flat_map(|len| (vec(any::<u8>(), len), vec(any::<u8>(), len)))
    }

    fn bit2_dot_case() -> impl Strategy<Value = (Vec<i8>, Vec<u8>)> {
        (1_usize..=4_096)
            .prop_flat_map(|len| (vec(any::<i8>(), len), vec(any::<u8>(), len.div_ceil(4))))
    }

    fn bit4_dot_case() -> impl Strategy<Value = (Vec<i8>, Vec<u8>)> {
        (1_usize..=4_096)
            .prop_flat_map(|len| (vec(any::<i8>(), len), vec(any::<u8>(), len.div_ceil(2))))
    }

    fn prepare_bit4_kernel_query(codes: &[i8]) -> Vec<i8> {
        let mut prepared = Vec::with_capacity(codes.len());
        for block in codes.chunks(32) {
            prepared.extend(block.iter().step_by(2).copied());
            prepared.extend(block.iter().skip(1).step_by(2).copied());
        }
        prepared
    }

    fn vector_pair_f16() -> impl Strategy<Value = (Vec<u16>, Vec<u16>)> {
        (1_usize..=4_096).prop_flat_map(|len| (vec(any::<u16>(), len), vec(any::<u16>(), len)))
    }

    fn vector_pair_f32() -> impl Strategy<Value = (Vec<f32>, Vec<f32>)> {
        (1_usize..=4_096).prop_flat_map(|len| {
            (
                vec(-1_000.0_f32..=1_000.0_f32, len),
                vec(-1_000.0_f32..=1_000.0_f32, len),
            )
        })
    }

    fn ordered_f32_bits(value: f32) -> u32 {
        let bits = value.to_bits();
        if bits & 0x8000_0000 == 0 {
            bits | 0x8000_0000
        } else {
            !bits
        }
    }

    fn f16_to_f64(bits: u16) -> f64 {
        let sign = if bits & 0x8000 == 0 { 1.0 } else { -1.0 };
        let exponent = (bits >> 10) & 0x1f;
        let fraction = bits & 0x03ff;
        match exponent {
            0 => sign * f64::from(fraction) * 2.0_f64.powi(-24),
            0x1f if fraction == 0 => sign * f64::INFINITY,
            0x1f => f64::NAN,
            _ => {
                let significand = 1.0 + f64::from(fraction) / 1_024.0;
                sign * significand * 2.0_f64.powi(i32::from(exponent) - 15)
            }
        }
    }

    fn f32_dot_magnitude(a: &[f32], b: &[f32]) -> f64 {
        a.iter()
            .zip(b)
            .map(|(&left, &right)| f64::from(left).abs() * f64::from(right).abs())
            .sum()
    }

    fn f16_dot_magnitude(a: &[u16], b: &[u16]) -> f64 {
        a.iter()
            .zip(b)
            .map(|(&left, &right)| f16_to_f64(left).abs() * f16_to_f64(right).abs())
            .sum()
    }

    fn f16_dot_reference(a: &[u16], b: &[u16]) -> f64 {
        a.iter()
            .zip(b)
            .map(|(&left, &right)| f16_to_f64(left) * f16_to_f64(right))
            .sum()
    }

    fn f32_dot_reference(a: &[f32], b: &[f32]) -> f64 {
        a.iter()
            .zip(b)
            .map(|(&left, &right)| f64::from(left) * f64::from(right))
            .sum()
    }

    fn assert_f16_result_matches(a: &[u16], b: &[u16], expected: f32, actual: f32) {
        if expected.is_nan() {
            assert!(actual.is_nan(), "expected NaN, got {actual:?}");
        } else if expected.is_infinite() {
            assert_eq!(actual, expected, "infinity sign changed");
        } else {
            assert!(actual.is_finite(), "finite oracle became {actual:?}");
            let ulps = ordered_f32_bits(expected).abs_diff(ordered_f32_bits(actual));
            let magnitude = f16_dot_magnitude(a, b);
            // As with f32, cancellation can make |result| arbitrarily smaller
            // than the terms. Preserve the existing one-ULP acceptance, then
            // apply the fixed backward-error epsilon to SUM|a_i * b_i|.
            let tolerance = 1.0e-5_f64 * magnitude.max(1.0);
            let error = (f64::from(actual) - f64::from(expected)).abs();
            let reference = f16_dot_reference(a, b);
            let actual_error = (f64::from(actual) - reference).abs();
            assert!(
                ulps <= 1 || error <= tolerance || actual_error <= tolerance,
                "f16 dot exceeded its one-ULP/backward-error bound: ulps={ulps} expected={expected:?} actual={actual:?} error={error:?} tolerance={tolerance:?} actual_error={actual_error:?}"
            );
        }
    }

    fn assert_f32_result_matches(a: &[f32], b: &[f32], expected: f32, actual: f32) {
        // This is the dot-product backward-error model. SIMD reassociation is
        // bounded by SUM|a_i * b_i|, computed in f64 so the tolerance cannot
        // itself cancel. Scaling by |result| is wrong under cancellation. If
        // the rounded scalar oracle and SIMD result straddle the reference,
        // the SIMD candidate is checked directly against the f64 dot.
        let magnitude = f32_dot_magnitude(a, b);
        let tolerance = 1.0e-5_f64 * magnitude.max(1.0);
        let error = (f64::from(actual) - f64::from(expected)).abs();
        let reference = f32_dot_reference(a, b);
        let actual_error = (f64::from(actual) - reference).abs();
        assert!(
            error <= tolerance || actual_error <= tolerance,
            "f32 dot exceeded backward-error tolerance: expected={expected:?} actual={actual:?} error={error:?} tolerance={tolerance:?} actual_error={actual_error:?}"
        );
    }

    proptest! {
        #![proptest_config(ProptestConfig {
            rng_seed: RngSeed::Fixed(0x5eed_03b1_0204),
            failure_persistence: Some(Box::new(
                proptest::test_runner::FileFailurePersistence::Direct(concat!(
                    env!("CARGO_MANIFEST_DIR"),
                    "/tests/kernels.proptest-regressions"
                )),
            )),
            ..ProptestConfig::default()
        })]

        #[test]
        fn prop_neon_i8_dot_equals_scalar((a, b) in vector_pair_i8()) {
            let scalar = KernelVariant::scalar().dot_i8(&a, &b);
            for variant in KernelVariant::available() {
                prop_assert_eq!(variant.dot_i8(&a, &b), scalar, "arm={:?}", variant.arm());
            }
        }

        #[test]
        fn prop_hamming_equals_scalar((a, b) in vector_pair_u8()) {
            let scalar = KernelVariant::scalar().hamming_u1(&a, &b);
            for variant in KernelVariant::available() {
                prop_assert_eq!(variant.hamming_u1(&a, &b), scalar, "arm={:?}", variant.arm());
            }
        }

        #[test]
        fn prop_f16_dot_equals_scalar_within_one_ulp((a, b) in vector_pair_f16()) {
            let scalar = KernelVariant::scalar().dot_f16(&a, &b);
            for variant in KernelVariant::available() {
                assert_f16_result_matches(&a, &b, scalar, variant.dot_f16(&a, &b));
            }
        }

        #[test]
        fn prop_f32_dot_equals_scalar_with_relative_epsilon((a, b) in vector_pair_f32()) {
            let scalar = KernelVariant::scalar().dot_f32(&a, &b);
            for variant in KernelVariant::available() {
                assert_f32_result_matches(&a, &b, scalar, variant.dot_f32(&a, &b));
            }
        }

        #[test]
        fn prop_bit2_all_arms_equal_scalar((q, codes) in bit2_dot_case()) {
            let scalar = KernelVariant::scalar().dot_bit2(&q, &codes);
            for variant in KernelVariant::available() {
                prop_assert_eq!(variant.dot_bit2(&q, &codes), scalar, "arm={:?}", variant.arm());
            }
        }

        #[test]
        fn prop_bit4_all_arms_equal_scalar((q, codes) in bit4_dot_case()) {
            let scalar = KernelVariant::scalar().dot_bit4(&q, &codes);
            for variant in KernelVariant::available() {
                prop_assert_eq!(variant.dot_bit4(&q, &codes), scalar, "arm={:?}", variant.arm());
            }
        }

        #[test]
        fn prop_bit4_prepared_all_arms_equal_scalar((q, codes) in bit4_dot_case()) {
            let scalar = KernelVariant::scalar().dot_bit4(&q, &codes);
            let query_sum = q.iter().map(|&code| i32::from(code)).sum();
            let prepared = prepare_bit4_kernel_query(&q);
            for variant in KernelVariant::available() {
                prop_assert_eq!(
                    variant.dot_bit4_prepared(&prepared, query_sum, &codes),
                    scalar,
                    "arm={:?}",
                    variant.arm()
                );
            }
        }

        #[test]
        fn prop_bit2_batch_equals_n_single_calls(
            d in 1_usize..=4_096,
            row_count in 1_usize..=4,
            q_seed in any::<i8>(),
            row_seed in any::<u8>(),
        ) {
            let q: Vec<i8> = (0..d)
                .map(|index| q_seed.wrapping_add((index.wrapping_mul(29)) as i8))
                .collect();
            let row_bytes = d.div_ceil(4);
            let rows: Vec<u8> = (0..row_bytes * row_count)
                .map(|index| row_seed.wrapping_add((index.wrapping_mul(31)) as u8))
                .collect();
            for variant in KernelVariant::available() {
                let mut actual = vec![0_i32; row_count];
                variant.dot_bit2_batch(&q, &rows, d, &mut actual);
                let expected: Vec<i32> = rows
                    .chunks_exact(row_bytes)
                    .map(|row| variant.dot_bit2(&q, row))
                    .collect();
                prop_assert_eq!(actual, expected, "arm={:?}", variant.arm());
            }
        }

        #[test]
        fn prop_bit4_batch_equals_n_single_calls(
            d in 1_usize..=4_096,
            row_count in 1_usize..=4,
            q_seed in any::<i8>(),
            row_seed in any::<u8>(),
        ) {
            let q: Vec<i8> = (0..d)
                .map(|index| q_seed.wrapping_add((index.wrapping_mul(29)) as i8))
                .collect();
            let row_bytes = d.div_ceil(2);
            let rows: Vec<u8> = (0..row_bytes * row_count)
                .map(|index| row_seed.wrapping_add((index.wrapping_mul(31)) as u8))
                .collect();
            for variant in KernelVariant::available() {
                let mut actual = vec![0_i32; row_count];
                variant.dot_bit4_batch(&q, &rows, d, &mut actual);
                let expected: Vec<i32> = rows
                    .chunks_exact(row_bytes)
                    .map(|row| variant.dot_bit4(&q, row))
                    .collect();
                prop_assert_eq!(actual, expected, "arm={:?}", variant.arm());
            }
        }

        #[test]
        fn prop_bit4_prepared_score_batch_equals_scalar(
            d in 1_usize..=1_024,
            row_count in 1_usize..=7,
            q_seed in any::<i8>(),
            row_seed in any::<u8>(),
        ) {
            let q: Vec<i8> = (0..d)
                .map(|index| q_seed.wrapping_add((index.wrapping_mul(29)) as i8))
                .collect();
            let query_sum = q.iter().map(|&code| i32::from(code)).sum();
            let prepared = prepare_bit4_kernel_query(&q);
            let row_bytes = d.div_ceil(2);
            let rows: Vec<u8> = (0..row_bytes * row_count)
                .map(|index| row_seed.wrapping_add((index.wrapping_mul(31)) as u8))
                .collect();
            let factor_source = (0..d)
                .map(|index| ((index % 17) as f32 - 8.0) / 9.0)
                .collect::<Vec<_>>();
            let mut ignored_codes = vec![0_u8; row_bytes];
            let factor = quantize_bit4(&factor_source, &mut ignored_codes).expect("valid row");
            let factors = vec![factor; row_count];
            let scalar = KernelVariant::scalar();
            let mut expected = vec![f32::NAN; row_count];
            scalar.score_bit4_prepared_batch(
                (&prepared, query_sum, 0.125),
                &rows,
                d,
                &factors,
                &mut expected,
            );
            for variant in KernelVariant::available() {
                let mut actual = vec![f32::NAN; row_count];
                variant.score_bit4_prepared_batch(
                    (&prepared, query_sum, 0.125),
                    &rows,
                    d,
                    &factors,
                    &mut actual,
                );
                prop_assert_eq!(
                    actual.iter().map(|score| score.to_bits()).collect::<Vec<_>>(),
                    expected.iter().map(|score| score.to_bits()).collect::<Vec<_>>(),
                    "arm={:?} tier={:?}",
                    variant.arm(),
                    variant.tier(),
                );
            }
        }

        #[test]
        fn prop_batch_equals_n_single_calls(
            d in 1_usize..=256,
            row_count in 1_usize..=8,
            q_seed in any::<i8>(),
            row_seed in any::<i8>(),
            hamming_q_seed in any::<u8>(),
            hamming_row_seed in any::<u8>(),
        ) {
            let q: Vec<i8> = (0..d)
                .map(|index| q_seed.wrapping_add(index as i8))
                .collect();
            let rows: Vec<i8> = (0..d * row_count)
                .map(|index| row_seed.wrapping_add((index.wrapping_mul(31)) as i8))
                .collect();
            let hamming_q: Vec<u8> = (0..d)
                .map(|index| hamming_q_seed.wrapping_add(index as u8))
                .collect();
            let hamming_rows: Vec<u8> = (0..d * row_count)
                .map(|index| hamming_row_seed.wrapping_add((index.wrapping_mul(17)) as u8))
                .collect();

            for variant in KernelVariant::available() {
                let mut i8_actual = vec![0_i32; row_count];
                variant.dot_i8_batch(&q, &rows, d, &mut i8_actual);
                let i8_expected: Vec<i32> = rows
                    .chunks_exact(d)
                    .map(|row| variant.dot_i8(&q, row))
                    .collect();
                prop_assert_eq!(i8_actual, i8_expected, "arm={:?}", variant.arm());

                let mut hamming_actual = vec![0_u32; row_count];
                variant.hamming_u1_batch(&hamming_q, &hamming_rows, d, &mut hamming_actual);
                let hamming_expected: Vec<u32> = hamming_rows
                    .chunks_exact(d)
                    .map(|row| variant.hamming_u1(&hamming_q, row))
                    .collect();
                prop_assert_eq!(hamming_actual, hamming_expected, "arm={:?}", variant.arm());
            }
        }
    }

    #[test]
    fn public_api_matches_scalar_oracle() {
        let i8_a = [-128, -17, 0, 11, 127];
        let i8_b = [-128, 19, -1, 7, 127];
        let f32_a = [1.0, -2.0, 3.5, 0.25, -0.0];
        let f32_b = [0.5, 4.0, -2.0, 8.0, 12.0];
        let f16_a = [0x0001, 0x3c00, 0xbc00, 0x7bff];
        let f16_b = [0x0001, 0x4000, 0x3800, 0x0400];
        let bits_a = [0b1010_0001, 0b1111_0000, 0b0000_0011];
        let bits_b = [0b0011_0001, 0b0101_1010, 0b0000_0000];
        let scalar = KernelVariant::scalar();

        assert_eq!(dot_i8(&i8_a, &i8_b), scalar.dot_i8(&i8_a, &i8_b));
        assert_f32_result_matches(
            &f32_a,
            &f32_b,
            scalar.dot_f32(&f32_a, &f32_b),
            dot_f32(&f32_a, &f32_b),
        );
        assert_f16_result_matches(
            &f16_a,
            &f16_b,
            scalar.dot_f16(&f16_a, &f16_b),
            dot_f16(&f16_a, &f16_b),
        );
        assert_eq!(
            hamming_u1(&bits_a, &bits_b),
            scalar.hamming_u1(&bits_a, &bits_b)
        );

        let rows = [i8_b, i8_a].concat();
        let mut i8_out = [0_i32; 2];
        dot_i8_batch(&i8_a, &rows, i8_a.len(), &mut i8_out);
        assert_eq!(
            i8_out,
            [scalar.dot_i8(&i8_a, &i8_b), scalar.dot_i8(&i8_a, &i8_a)]
        );

        let bit_rows = [bits_b, bits_a].concat();
        let mut hamming_out = [0_u32; 2];
        hamming_u1_batch(&bits_a, &bit_rows, bits_a.len(), &mut hamming_out);
        assert_eq!(
            hamming_out,
            [
                scalar.hamming_u1(&bits_a, &bits_b),
                scalar.hamming_u1(&bits_a, &bits_a)
            ]
        );
    }

    #[test]
    fn packed_dot_public_surface_matches_worked_examples() {
        let bit2_query = [1_i8, 2, 3, 4, 5];
        let bit2_codes = [0b0001_1011_u8, 0b1000_0000];
        let bit4_query = [2_i8, -3, 4];
        let bit4_codes = [0x0f_u8, 0x80];
        let scalar = KernelVariant::scalar();

        assert_eq!(scalar.dot_bit2(&bit2_query, &bit2_codes), 15);
        assert_eq!(dot_bit2(&bit2_query, &bit2_codes), 15);
        assert_eq!(scalar.dot_bit4(&bit4_query, &bit4_codes), -71);
        assert_eq!(dot_bit4(&bit4_query, &bit4_codes), -71);
    }

    #[test]
    fn packed_dot_batches_match_worked_single_calls() {
        let bit2_query = [1_i8, 2, 3, 4, 5];
        let bit2_rows = [0b0001_1011_u8, 0b1000_0000, 0b1110_0100, 0b0100_0000];
        let bit4_query = [2_i8, -3, 4];
        let bit4_rows = [0x0f_u8, 0x80, 0xf0, 0x70];

        let mut bit2_out = [0_i32; 2];
        dot_bit2_batch(&bit2_query, &bit2_rows, bit2_query.len(), &mut bit2_out);
        assert_eq!(
            bit2_out,
            [
                dot_bit2(&bit2_query, &bit2_rows[..2]),
                dot_bit2(&bit2_query, &bit2_rows[2..]),
            ]
        );

        let mut bit4_out = [0_i32; 2];
        dot_bit4_batch(&bit4_query, &bit4_rows, bit4_query.len(), &mut bit4_out);
        assert_eq!(
            bit4_out,
            [
                dot_bit4(&bit4_query, &bit4_rows[..2]),
                dot_bit4(&bit4_query, &bit4_rows[2..]),
            ]
        );
    }

    #[test]
    fn packed_dot_tails_and_byte_boundaries_equal_scalar() {
        let lengths = [
            1_usize, 2, 3, 4, 5, 7, 15, 16, 17, 31, 32, 33, 63, 64, 65, 67, 127, 128, 129, 257,
            4_093, 4_096,
        ];
        for len in lengths {
            let q: Vec<i8> = (0..len)
                .map(|index| (index.wrapping_mul(43) as i8).wrapping_sub(91))
                .collect();
            let bit2_codes: Vec<u8> = (0..len.div_ceil(4))
                .map(|index| (index.wrapping_mul(71) as u8).wrapping_add(19))
                .collect();
            let bit4_codes: Vec<u8> = (0..len.div_ceil(2))
                .map(|index| (index.wrapping_mul(53) as u8).wrapping_add(7))
                .collect();
            let bit2_scalar = KernelVariant::scalar().dot_bit2(&q, &bit2_codes);
            let bit4_scalar = KernelVariant::scalar().dot_bit4(&q, &bit4_codes);
            for variant in KernelVariant::available() {
                assert_eq!(
                    variant.dot_bit2(&q, &bit2_codes),
                    bit2_scalar,
                    "Bit2 len={len} arm={:?}",
                    variant.arm()
                );
                assert_eq!(
                    variant.dot_bit4(&q, &bit4_codes),
                    bit4_scalar,
                    "Bit4 len={len} arm={:?}",
                    variant.arm()
                );
            }
        }
    }

    #[test]
    fn packed_dot_ignores_unused_trailing_fields() {
        let bit2_query = [3_i8, -5, 7, -11, 13];
        let bit4_query = [3_i8, -5, 7];
        for variant in KernelVariant::available() {
            assert_eq!(
                variant.dot_bit2(&bit2_query, &[0x1b, 0x80]),
                variant.dot_bit2(&bit2_query, &[0x1b, 0xbf]),
                "Bit2 arm={:?}",
                variant.arm()
            );
            assert_eq!(
                variant.dot_bit4(&bit4_query, &[0x1f, 0x80]),
                variant.dot_bit4(&bit4_query, &[0x1f, 0x8f]),
                "Bit4 arm={:?}",
                variant.arm()
            );
        }
    }

    #[test]
    fn i8_min_and_non_lane_tails_are_exact() {
        let lengths = [1_usize, 3, 7, 15, 16, 17, 31, 32, 33, 127, 257, 4_093];
        for len in lengths {
            let a: Vec<i8> = (0..len)
                .map(|index| if index % 3 == 0 { i8::MIN } else { index as i8 })
                .collect();
            let b: Vec<i8> = (0..len)
                .map(|index| {
                    if index % 5 == 0 {
                        i8::MIN
                    } else {
                        (index * 17) as i8
                    }
                })
                .collect();
            let scalar = KernelVariant::scalar().dot_i8(&a, &b);
            for variant in KernelVariant::available() {
                assert_eq!(
                    variant.dot_i8(&a, &b),
                    scalar,
                    "len={len} arm={:?}",
                    variant.arm()
                );
            }
        }
    }

    #[test]
    fn i8_accumulator_bound_and_max_dimension_guard_hold() {
        let a = vec![i8::MIN; 4_096];
        let b = vec![i8::MIN; 4_096];
        assert_eq!(dot_i8(&a, &b), 1_i32 << 26);
        assert_eq!(4_096_i64 * 16_384_i64, 1_i64 << 26);
        assert!(MAX_DOT_I8_DIMENSION as i64 * 16_384_i64 <= i64::from(i32::MAX));

        let max_a = vec![i8::MIN; MAX_DOT_I8_DIMENSION];
        let max_b = vec![i8::MIN; MAX_DOT_I8_DIMENSION];
        assert_eq!(dot_i8(&max_a, &max_b), 1_i32 << 30);
    }

    #[test]
    fn bit4_prepared_max_dimension_intermediates_remain_exact() {
        let q = vec![i8::MIN; MAX_DOT_I8_DIMENSION];
        let codes = vec![0xff_u8; MAX_DOT_I8_DIMENSION / 2];
        let prepared = prepare_bit4_kernel_query(&q);
        let query_sum = q.iter().map(|&code| i32::from(code)).sum();
        let expected = KernelVariant::scalar().dot_bit4(&q, &codes);
        assert_eq!(expected, -125_829_120);
        for variant in KernelVariant::available() {
            assert_eq!(
                variant.dot_bit4_prepared(&prepared, query_sum, &codes),
                expected,
                "arm={:?}",
                variant.arm()
            );
        }
    }

    #[cfg(target_arch = "aarch64")]
    #[test]
    fn i8mm_variant_is_registered_and_batch_layout_matches_scalar() {
        if !detected_features().i8mm {
            return;
        }
        let variant = KernelVariant::available()
            .find(|variant| variant.tier() == InstructionTier::NeonI8mmReserved)
            .expect("runtime I8MM support materializes the campaign tier");
        for (d, row_count) in [
            (1_usize, 2_usize),
            (7, 3),
            (8, 2),
            (15, 5),
            (16, 2),
            (31, 3),
            (257, 5),
        ] {
            let q: Vec<i8> = (0..d)
                .map(|index| if index % 5 == 0 { i8::MIN } else { index as i8 })
                .collect();
            let rows: Vec<i8> = (0..d * row_count)
                .map(|index| {
                    if index % 7 == 0 {
                        i8::MIN
                    } else {
                        index.wrapping_mul(31) as i8
                    }
                })
                .collect();
            let mut actual = vec![0_i32; row_count];
            variant.dot_i8_batch(&q, &rows, d, &mut actual);
            let expected = rows
                .chunks_exact(d)
                .map(|row| KernelVariant::scalar().dot_i8(&q, row))
                .collect::<Vec<_>>();
            assert_eq!(actual, expected, "d={d} rows={row_count}");
        }
    }

    #[test]
    fn f16_special_values_have_defined_ieee_behavior() {
        let scalar = KernelVariant::scalar();
        let subnormal = scalar.dot_f16(&[0x0001], &[0x3c00]);
        assert_eq!(subnormal.to_bits(), (2.0_f32.powi(-24)).to_bits());
        assert_eq!(scalar.dot_f16(&[0x7c00], &[0x3c00]), f32::INFINITY);
        assert_eq!(scalar.dot_f16(&[0xfc00], &[0x3c00]), f32::NEG_INFINITY);
        assert!(scalar.dot_f16(&[0x7e01], &[0x3c00]).is_nan());
        assert!(
            scalar
                .dot_f16(&[0x7c00, 0xfc00], &[0x3c00, 0x3c00])
                .is_nan()
        );

        let a = [0x0001, 0x03ff, 0x7c00, 0xfc00, 0x7e01];
        let b = [0x3c00; 5];
        let expected = scalar.dot_f16(&a, &b);
        for variant in KernelVariant::available() {
            assert_f16_result_matches(&a, &b, expected, variant.dot_f16(&a, &b));
        }
    }

    #[test]
    fn hamming_counts_all_bits_in_final_packed_byte() {
        // The API receives a byte length, not a logical bit length, so every
        // bit in the final byte is significant, including padding chosen by a
        // caller whose logical dimension is not divisible by eight.
        let a = [0_u8, 0b1110_0000];
        let b = [0_u8, 0b0000_0000];
        for variant in KernelVariant::available() {
            assert_eq!(variant.hamming_u1(&a, &b), 3, "arm={:?}", variant.arm());
        }
    }

    #[test]
    fn empty_batches_are_defined_without_panics() {
        let mut i8_out = [7_i32; 2];
        dot_i8_batch(&[], &[], 0, &mut i8_out);
        assert_eq!(i8_out, [0, 0]);

        let mut hamming_out = [7_u32; 2];
        hamming_u1_batch(&[], &[], 0, &mut hamming_out);
        assert_eq!(hamming_out, [0, 0]);

        let mut bit2_out = [7_i32; 2];
        dot_bit2_batch(&[], &[], 0, &mut bit2_out);
        assert_eq!(bit2_out, [0, 0]);

        let mut bit4_out = [7_i32; 2];
        dot_bit4_batch(&[], &[], 0, &mut bit4_out);
        assert_eq!(bit4_out, [0, 0]);
    }

    #[test]
    fn dispatch_forced_arm_override_is_typed_and_never_executes_unsupported_code() {
        let child_mode = std::env::var_os("ZE_KERNEL_TEST_CHILD");
        if child_mode.as_deref() == Some(std::ffi::OsStr::new("default")) {
            assert!(initialize().is_ok());
            return;
        }
        if child_mode.as_deref() == Some(std::ffi::OsStr::new("same")) {
            let first = initialize().expect("first scalar initialization succeeds");
            assert_eq!(initialize(), Ok(first));
            return;
        }
        if child_mode.as_deref() == Some(std::ffi::OsStr::new("conflict")) {
            let automatic = selected_arm();
            let result = initialize();
            if automatic == KernelArm::Scalar {
                assert_eq!(result, Ok(KernelArm::Scalar));
            } else {
                assert_eq!(
                    result,
                    Err(KernelInitError::AlreadyInitialized {
                        selected: automatic,
                        requested: KernelArm::Scalar,
                    })
                );
            }
            return;
        }
        #[cfg(unix)]
        if child_mode.as_deref() == Some(std::ffi::OsStr::new("nonunicode")) {
            assert_eq!(initialize(), Err(KernelInitError::NonUnicodeOverride));
            return;
        }
        if child_mode.is_some() {
            let forced = std::env::var("ZE_KERNEL").expect("child receives ZE_KERNEL");
            let requested = match forced.as_str() {
                "scalar" => KernelArm::Scalar,
                "neon" => KernelArm::Neon,
                "avx2" => KernelArm::Avx2,
                other => {
                    let error = initialize().expect_err("unknown override must be rejected");
                    assert!(
                        matches!(error, KernelInitError::UnknownOverride { value } if value == other)
                    );
                    return;
                }
            };

            if is_arm_supported(requested) {
                assert_eq!(
                    initialize().expect("supported forced arm initializes"),
                    requested
                );
                assert_eq!(selected_arm(), requested);
                assert_eq!(dot_i8(&[-128, 7], &[-128, -9]), 16_321);
            } else {
                let error = initialize().expect_err("unsupported forced arm must be rejected");
                assert!(
                    matches!(error, KernelInitError::UnsupportedArm { requested: arm } if arm == requested)
                );
            }
            return;
        }

        let executable = std::env::current_exe().expect("test executable is available");
        for forced in ["scalar", "neon", "avx2", "unknown"] {
            let status = Command::new(&executable)
                .arg("--exact")
                .arg("kernels::dispatch_forced_arm_override_is_typed_and_never_executes_unsupported_code")
                .arg("--nocapture")
                .env("ZE_KERNEL_TEST_CHILD", "1")
                .env("ZE_KERNEL", forced)
                .status()
                .expect("forced-arm child starts");
            assert!(status.success(), "forced-arm child failed for {forced}");
        }

        for mode in ["default", "same", "conflict"] {
            let mut command = Command::new(&executable);
            command
                .arg("--exact")
                .arg("kernels::dispatch_forced_arm_override_is_typed_and_never_executes_unsupported_code")
                .arg("--nocapture")
                .env("ZE_KERNEL_TEST_CHILD", mode);
            if mode == "default" {
                command.env_remove("ZE_KERNEL");
            } else {
                command.env("ZE_KERNEL", "scalar");
            }
            let status = command.status().expect("dispatch-state child starts");
            assert!(status.success(), "dispatch-state child failed for {mode}");
        }

        #[cfg(unix)]
        {
            use std::os::unix::ffi::OsStringExt as _;
            let status = Command::new(&executable)
                .arg("--exact")
                .arg("kernels::dispatch_forced_arm_override_is_typed_and_never_executes_unsupported_code")
                .arg("--nocapture")
                .env("ZE_KERNEL_TEST_CHILD", "nonunicode")
                .env("ZE_KERNEL", std::ffi::OsString::from_vec(vec![0xff]))
                .status()
                .expect("non-Unicode override child starts");
            assert!(status.success(), "non-Unicode forced-arm child failed");
        }
    }

    #[test]
    #[cfg(target_arch = "aarch64")]
    fn aarch64_runtime_exposes_neon_variant() {
        assert!(is_arm_supported(KernelArm::Neon));
        let tiers: Vec<InstructionTier> = KernelVariant::available()
            .filter(|variant| variant.arm() == KernelArm::Neon)
            .map(KernelVariant::tier)
            .collect();
        assert!(tiers.contains(&InstructionTier::NeonWiden));
        if std::arch::is_aarch64_feature_detected!("dotprod") {
            assert!(tiers.contains(&InstructionTier::NeonDotprod));
        }
    }

    #[test]
    fn knob_space_is_registered_as_data_for_task_27_h() {
        assert_eq!(KERNEL_KNOB_SPACE.unroll, [2, 4, 6, 8]);
        assert_eq!(KERNEL_KNOB_SPACE.accumulators, [2, 4, 6, 8]);
        assert_eq!(KERNEL_KNOB_SPACE.rows_per_block, [1, 2, 4, 8, 16]);
        assert_eq!(KERNEL_KNOB_SPACE.prefetch_dist, [0, 1, 2, 4, 8]);
        assert_eq!(
            KERNEL_KNOB_SPACE.tier,
            [
                InstructionTier::Scalar,
                InstructionTier::NeonWiden,
                InstructionTier::NeonDotprod,
                InstructionTier::Avx2,
                InstructionTier::NeonI8mmReserved,
                InstructionTier::Sme2Reserved,
            ]
        );
    }

    #[test]
    fn dispatch_errors_and_variants_have_actionable_debug_text() {
        let errors = [
            KernelInitError::UnknownOverride {
                value: String::from("bogus"),
            },
            KernelInitError::NonUnicodeOverride,
            KernelInitError::UnsupportedArm {
                requested: KernelArm::Avx2,
            },
            KernelInitError::AlreadyInitialized {
                selected: KernelArm::Scalar,
                requested: KernelArm::Neon,
            },
        ];
        for error in errors {
            assert!(!error.to_string().is_empty());
        }
        assert!(format!("{:?}", KernelVariant::scalar()).contains("Scalar"));
        let _features = zeppelin_embed::kernels::detected_features();

        let scalar = KernelVariant::scalar();
        let mut i8_out = [9_i32; 2];
        scalar.dot_i8_batch(&[], &[], 0, &mut i8_out);
        assert_eq!(i8_out, [0, 0]);
        let mut hamming_out = [9_u32; 2];
        scalar.hamming_u1_batch(&[], &[], 0, &mut hamming_out);
        assert_eq!(hamming_out, [0, 0]);
    }

    #[test]
    #[cfg(target_arch = "x86_64")]
    fn x86_runtime_executes_avx2_when_host_supports_it() {
        assert_eq!(
            is_arm_supported(KernelArm::Avx2),
            std::arch::is_x86_feature_detected!("avx2")
                && std::arch::is_x86_feature_detected!("popcnt")
        );
        let variant = KernelVariant::available().find(|variant| variant.arm() == KernelArm::Avx2);
        if std::env::var_os("ZE_REQUIRE_AVX2").is_some() {
            assert!(variant.is_some(), "CI runner must expose AVX2+POPCNT");
        }
        if let Some(variant) = variant {
            assert_eq!(variant.tier(), InstructionTier::Avx2);
            assert_eq!(
                variant.dot_i8(&[-128, -1, 0, 1, 127], &[-128, 3, 9, -7, 127]),
                32_503
            );
            assert_eq!(variant.hamming_u1(&[0xff, 0x00], &[0x0f, 0xf0]), 8);
        }
    }

    fn deterministic_i8(index: usize) -> i8 {
        ((index.wrapping_mul(31).wrapping_add(17) % 255) as i16 - 127) as i8
    }

    fn deterministic_u8(index: usize) -> u8 {
        index.wrapping_mul(37).wrapping_add(11) as u8
    }

    #[test]
    #[ignore = "wall-clock performance gate; run release-mode on macOS CI"]
    fn perf_i8_batch_768_100k_is_at_least_8_gbps() {
        const D: usize = 768;
        const ROWS: usize = 100_000;
        let q: Vec<i8> = (0..D).map(deterministic_i8).collect();
        let rows: Vec<i8> = (0..D * ROWS).map(deterministic_i8).collect();
        let mut out = vec![0_i32; ROWS];

        dot_i8_batch(&q, &rows, D, &mut out);
        let started = Instant::now();
        dot_i8_batch(&q, &rows, D, &mut out);
        let elapsed = started.elapsed();
        let gbps = rows.len() as f64 / elapsed.as_secs_f64() / 1.0e9;
        eprintln!(
            "TASK03_PERF i8_batch d={D} rows={ROWS} elapsed_ns={} gbps={gbps:.9}",
            elapsed.as_nanos()
        );
        assert!(
            gbps >= 8.0,
            "i8 batch throughput {gbps:.6} GB/s is below 8 GB/s"
        );
        std::hint::black_box(out);
    }

    #[test]
    #[ignore = "wall-clock performance gate; run release-mode on macOS CI"]
    fn perf_hamming_batch_768_1m_is_at_most_10ms() {
        const D_BYTES: usize = 96;
        const ROWS: usize = 1_000_000;
        let q: Vec<u8> = (0..D_BYTES).map(deterministic_u8).collect();
        let rows: Vec<u8> = (0..D_BYTES * ROWS).map(deterministic_u8).collect();
        let mut out = vec![0_u32; ROWS];

        hamming_u1_batch(&q, &rows, D_BYTES, &mut out);
        let started = Instant::now();
        hamming_u1_batch(&q, &rows, D_BYTES, &mut out);
        let elapsed = started.elapsed();
        eprintln!(
            "TASK03_PERF hamming_batch d_bytes={D_BYTES} rows={ROWS} elapsed_ns={}",
            elapsed.as_nanos()
        );
        assert!(
            elapsed <= std::time::Duration::from_millis(10),
            "hamming batch took {elapsed:?}, above 10 ms"
        );
        std::hint::black_box(out);
    }
}
