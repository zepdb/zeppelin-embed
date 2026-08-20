#![no_main]

use libfuzzer_sys::fuzz_target;
use zeppelin_embed::kernels::KernelVariant;

fn fuzz_byte(payload: &[u8], index: usize) -> u8 {
    payload
        .get(index % payload.len())
        .copied()
        .unwrap_or_default()
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

fn f16_magnitude(a: &[u16], b: &[u16]) -> f64 {
    a.iter()
        .zip(b)
        .map(|(&left, &right)| f16_to_f64(left).abs() * f16_to_f64(right).abs())
        .sum()
}

fn f16_reference(a: &[u16], b: &[u16]) -> f64 {
    a.iter()
        .zip(b)
        .map(|(&left, &right)| f16_to_f64(left) * f16_to_f64(right))
        .sum()
}

fn f32_magnitude(a: &[f32], b: &[f32]) -> f64 {
    a.iter()
        .zip(b)
        .map(|(&left, &right)| f64::from(left).abs() * f64::from(right).abs())
        .sum()
}

fn f32_reference(a: &[f32], b: &[f32]) -> f64 {
    a.iter()
        .zip(b)
        .map(|(&left, &right)| f64::from(left) * f64::from(right))
        .sum()
}

fn f16_matches(a: &[u16], b: &[u16], expected: f32, actual: f32) -> bool {
    if expected.is_nan() {
        actual.is_nan()
    } else if expected.is_infinite() {
        actual == expected
    } else {
        let ulps = ordered_f32_bits(expected).abs_diff(ordered_f32_bits(actual));
        // Dot-product backward error scales with SUM|a_i * b_i|, not with
        // |result|, which can be arbitrarily small after cancellation.
        let tolerance = 1.0e-5_f64 * f16_magnitude(a, b).max(1.0);
        let error = (f64::from(actual) - f64::from(expected)).abs();
        let reference = f16_reference(a, b);
        let actual_error = (f64::from(actual) - reference).abs();
        actual.is_finite() && (ulps <= 1 || error <= tolerance || actual_error <= tolerance)
    }
}

fn f32_matches(a: &[f32], b: &[f32], expected: f32, actual: f32) -> bool {
    let magnitude = f32_magnitude(a, b);
    let tolerance = 1.0e-5_f64 * magnitude.max(1.0);
    let oracle_error = (f64::from(actual) - f64::from(expected)).abs();
    if oracle_error <= tolerance {
        return true;
    }

    // Two rounded results may sit on opposite sides of the reference and
    // differ by more than one error budget. Arbitrate against the f64 dot and
    // require the SIMD candidate itself to satisfy the unchanged bound.
    let reference = f32_reference(a, b);
    let actual_reference_error = (f64::from(actual) - reference).abs();
    actual_reference_error <= tolerance
}

fuzz_target!(|data: &[u8]| {
    let Some(length_bytes) = data.get(..2) else {
        return;
    };
    let [length_low, length_high] = length_bytes else {
        return;
    };
    let Some(payload) = data.get(2..) else {
        return;
    };
    if payload.is_empty() {
        return;
    }
    let length_seed = u16::from_le_bytes([*length_low, *length_high]);
    let len = usize::from(length_seed) % 4_096 + 1;

    let i8_a: Vec<i8> = (0..len)
        .map(|index| fuzz_byte(payload, index) as i8)
        .collect();
    let i8_b: Vec<i8> = (0..len)
        .map(|index| fuzz_byte(payload, index.wrapping_mul(31).wrapping_add(7)) as i8)
        .collect();
    let hamming_a: Vec<u8> = (0..len).map(|index| fuzz_byte(payload, index)).collect();
    let hamming_b: Vec<u8> = (0..len)
        .map(|index| fuzz_byte(payload, index.wrapping_mul(13).wrapping_add(3)))
        .collect();
    let f16_a: Vec<u16> = (0..len)
        .map(|index| {
            u16::from_le_bytes([
                fuzz_byte(payload, index.wrapping_mul(2)),
                fuzz_byte(payload, index.wrapping_mul(2).wrapping_add(1)),
            ])
        })
        .collect();
    let f16_b: Vec<u16> = (0..len)
        .map(|index| {
            u16::from_le_bytes([
                fuzz_byte(payload, index.wrapping_mul(5).wrapping_add(2)),
                fuzz_byte(payload, index.wrapping_mul(7).wrapping_add(4)),
            ])
        })
        .collect();
    let f32_a: Vec<f32> = (0..len)
        .map(|index| (f32::from(fuzz_byte(payload, index)) - 127.5) / 16.0)
        .collect();
    let f32_b: Vec<f32> = (0..len)
        .map(|index| {
            (f32::from(fuzz_byte(payload, index.wrapping_mul(11).wrapping_add(1))) - 127.5) / 16.0
        })
        .collect();
    let bit4_codes: Vec<u8> = (0..len.div_ceil(2))
        .map(|index| fuzz_byte(payload, index.wrapping_mul(19).wrapping_add(9)))
        .collect();
    let row_count = usize::from(fuzz_byte(payload, 0) % 4) + 1;
    let bit4_rows: Vec<u8> = (0..bit4_codes.len() * row_count)
        .map(|index| fuzz_byte(payload, index.wrapping_mul(29).wrapping_add(13)))
        .collect();

    let scalar = KernelVariant::scalar();
    let expected_i8 = scalar.dot_i8(&i8_a, &i8_b);
    let expected_hamming = scalar.hamming_u1(&hamming_a, &hamming_b);
    let expected_f16 = scalar.dot_f16(&f16_a, &f16_b);
    let expected_f32 = scalar.dot_f32(&f32_a, &f32_b);
    let expected_bit4 = scalar.dot_bit4(&i8_a, &bit4_codes);
    let mut expected_bit4_batch = vec![0_i32; row_count];
    scalar.dot_bit4_batch(&i8_a, &bit4_rows, len, &mut expected_bit4_batch);
    for variant in KernelVariant::available() {
        assert_eq!(variant.dot_i8(&i8_a, &i8_b), expected_i8);
        assert_eq!(variant.hamming_u1(&hamming_a, &hamming_b), expected_hamming);
        let actual_f16 = variant.dot_f16(&f16_a, &f16_b);
        assert!(
            f16_matches(&f16_a, &f16_b, expected_f16, actual_f16),
            "f16 backward-error bound failed: expected={expected_f16:?} actual={actual_f16:?} reference={:?} magnitude={:?}",
            f16_reference(&f16_a, &f16_b),
            f16_magnitude(&f16_a, &f16_b)
        );
        let actual_f32 = variant.dot_f32(&f32_a, &f32_b);
        assert!(
            f32_matches(&f32_a, &f32_b, expected_f32, actual_f32),
            "f32 backward-error bound failed: expected={expected_f32:?} actual={actual_f32:?}"
        );
        assert_eq!(variant.dot_bit4(&i8_a, &bit4_codes), expected_bit4);

        let mut actual_bit4_batch = vec![0_i32; row_count];
        variant.dot_bit4_batch(&i8_a, &bit4_rows, len, &mut actual_bit4_batch);
        assert_eq!(actual_bit4_batch, expected_bit4_batch);
        for (row, &actual) in bit4_rows
            .chunks_exact(bit4_codes.len())
            .zip(&actual_bit4_batch)
        {
            assert_eq!(actual, variant.dot_bit4(&i8_a, row));
        }
    }
});
