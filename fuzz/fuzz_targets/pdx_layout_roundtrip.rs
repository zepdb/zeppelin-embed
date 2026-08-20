#![no_main]

use libfuzzer_sys::fuzz_target;
use zeppelin_embed::quant::QuantScheme;
use zeppelin_embed::scan::pdx::PdxMatrix;

fuzz_target!(|data: &[u8]| {
    let Some(header) = data.get(..5) else {
        return;
    };
    let [scheme_byte, dimension_low, dimension_high, rows_low, rows_high] = header else {
        return;
    };
    let Some(payload) = data.get(5..) else {
        return;
    };
    let schemes = [
        QuantScheme::F32,
        QuantScheme::F16,
        QuantScheme::Int8,
        QuantScheme::Bit4,
    ];
    let scheme = schemes
        .get(usize::from(*scheme_byte) % schemes.len())
        .copied()
        .unwrap_or_default();
    let dimension = usize::from(u16::from_le_bytes([*dimension_low, *dimension_high])) % 1_024 + 1;
    let row_count = usize::from(u16::from_le_bytes([*rows_low, *rows_high])) % 129;

    let Ok(matrix) = PdxMatrix::from_encoded_bytes(scheme, dimension, row_count, payload) else {
        return;
    };
    let reencoded = match scheme {
        QuantScheme::F32 => PdxMatrix::encode_f32(
            &matrix.decode_f32().expect("validated f32 PDX"),
            dimension,
        ),
        QuantScheme::F16 => PdxMatrix::encode_f16(
            &matrix.decode_f16().expect("validated f16 PDX"),
            dimension,
        ),
        QuantScheme::Int8 => PdxMatrix::encode_int8(
            &matrix.decode_int8().expect("validated Int8 PDX"),
            dimension,
        ),
        QuantScheme::Bit4 => PdxMatrix::encode_bit4(
            &matrix.decode_bit4().expect("validated Bit4 PDX"),
            dimension,
        ),
    }
    .expect("validated PDX re-encodes");
    assert_eq!(reencoded.encoded_bytes(), payload);
});
