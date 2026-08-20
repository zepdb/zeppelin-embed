#![no_main]

use libfuzzer_sys::fuzz_target;
use zeppelin_embed::quant::{
    Int8Vec, QuantError, dequantize_bit4, dequantize_int8, dot_int8_query, est_dot_bit4,
    prepare_bit4_query, prepare_int8_query, quantize_bit4, quantize_int8,
};

fuzz_target!(|data: &[u8]| {
    let coordinate_count = data.len().div_ceil(4).clamp(1, 4_096);
    let values = (0..coordinate_count)
        .map(|coordinate| {
            let start = coordinate * 4;
            let bytes = [
                data.get(start).copied().unwrap_or_default(),
                data.get(start + 1).copied().unwrap_or_default(),
                data.get(start + 2).copied().unwrap_or_default(),
                data.get(start + 3).copied().unwrap_or_default(),
            ];
            f32::from_bits(u32::from_le_bytes(bytes))
        })
        .collect::<Vec<_>>();

    let mut int8_codes = vec![0x2a_i8; values.len()];
    let mut bit4_codes = vec![0x3c_u8; values.len().div_ceil(2)];
    let int8 = quantize_int8(&values, &mut int8_codes);
    let bit4 = quantize_bit4(&values, &mut bit4_codes);

    if let Some(index) = values.iter().position(|value| !value.is_finite()) {
        assert_eq!(int8, Err(QuantError::NonFinite { index }));
        assert_eq!(bit4, Err(QuantError::NonFinite { index }));
        assert!(int8_codes.iter().all(|code| *code == 0x2a));
        assert!(bit4_codes.iter().all(|code| *code == 0x3c));
        return;
    }

    let (int8_scale, int8_offset) = int8.expect("finite int8 input");
    assert!(int8_scale.is_finite());
    assert!(int8_offset.is_finite());
    let bit4_factors = bit4.expect("finite bit4 input");
    for factor in [bit4_factors.norm(), bit4_factors.correction()] {
        assert!(factor.is_finite());
    }

    let mut int8_reconstructed = vec![f32::NAN; values.len()];
    dequantize_int8(
        Int8Vec {
            codes: &int8_codes,
            scale: int8_scale,
            offset: int8_offset,
        },
        &mut int8_reconstructed,
    )
    .expect("matching int8 output");
    let mut bit4_reconstructed = vec![f32::NAN; values.len()];
    dequantize_bit4(&bit4_codes, bit4_factors, &mut bit4_reconstructed)
        .expect("canonical bit4 code");
    assert!(int8_reconstructed.iter().all(|value| value.is_finite()));
    assert!(bit4_reconstructed.iter().all(|value| value.is_finite()));

    let int8_query = prepare_int8_query(&values).expect("finite int8 query");
    let bit4_query = prepare_bit4_query(&values, 4).expect("finite bit4 query");
    let _ = dot_int8_query(
        &int8_query,
        Int8Vec {
            codes: &int8_codes,
            scale: int8_scale,
            offset: int8_offset,
        },
    )
    .expect("matching int8 row");
    let _ = est_dot_bit4(&bit4_query, &bit4_codes, bit4_factors).expect("matching bit4 row");
});
