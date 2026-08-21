#![allow(
    clippy::expect_used,
    clippy::indexing_slicing,
    clippy::panic,
    clippy::unwrap_used
)]

use zeppelin_embed::format::frame::{
    FILE_HEADER_LEN, FILE_TRAILER_LEN, decode_artifact, encode_artifact,
};
use zeppelin_embed::format::golden::decode_hex;
use zeppelin_embed::format::{FormatFamily, FormatRegistry, RegistryError};
use zeppelin_embed::manifest::{EpochMeta, Manifest, decode_manifest, encode_manifest};
use zeppelin_embed::meta::{
    AliveSet, ColumnDefinition, ColumnId, ColumnInput, ColumnStoreBuilder, ColumnType, ColumnValue,
    Schema,
};
use zeppelin_embed::quant::quantize_bit4;
use zeppelin_embed::segment::SegmentId;
use zeppelin_embed::segment::layout::{Int8Factors, RegionKind};
use zeppelin_embed::segment::reader::SegmentReader;
use zeppelin_embed::segment::writer::{SegmentBuild, SegmentFactors, encode_segment};

fn fixture(text: &str) -> Vec<u8> {
    decode_hex(text).expect("fixture hex")
}

#[test]
fn format_frame_golden_is_byte_exact_and_semantically_exact() {
    let encoded = encode_artifact(FormatFamily::Frame, 0x1020_3040, b"golden");
    assert_eq!(
        encoded.len(),
        FILE_HEADER_LEN + 8 + b"golden".len() + 8 + FILE_TRAILER_LEN
    );
    assert_eq!(
        encoded,
        fixture(include_str!("fixtures/format/frame_v1.hex"))
    );
    let decoded = decode_artifact("frame.golden", FormatFamily::Frame, &encoded)
        .expect("golden frame decodes");
    assert_eq!(decoded.header.flags, 0x1020_3040);
    assert_eq!(decoded.payload, b"golden");
}

fn golden_segment(dims: u32, rows: u32, int8: bool) -> (Vec<u8>, SegmentId) {
    let schema = if rows == 0 {
        Schema::new(Vec::new()).expect("schema")
    } else {
        Schema::new(vec![
            ColumnDefinition::new(ColumnId::new(1), "u", ColumnType::U64, true),
            ColumnDefinition::new(ColumnId::new(2), "i", ColumnType::I64, true),
            ColumnDefinition::new(ColumnId::new(3), "f", ColumnType::F64, true),
            ColumnDefinition::new(ColumnId::new(4), "b", ColumnType::Bool, true),
            ColumnDefinition::new(ColumnId::new(5), "d", ColumnType::DictionaryString, true),
            ColumnDefinition::new(ColumnId::new(6), "r", ColumnType::RawString, true),
        ])
        .expect("schema")
    };
    let mut builder = ColumnStoreBuilder::new(schema);
    for row in 0..rows {
        builder
            .push_row(
                -7 + i64::from(row),
                &[
                    ColumnInput {
                        column: ColumnId::new(1),
                        value: ColumnValue::U64(9),
                    },
                    ColumnInput {
                        column: ColumnId::new(2),
                        value: ColumnValue::I64(-3),
                    },
                    ColumnInput {
                        column: ColumnId::new(3),
                        value: ColumnValue::F64(1.5),
                    },
                    ColumnInput {
                        column: ColumnId::new(4),
                        value: ColumnValue::Bool(true),
                    },
                    ColumnInput {
                        column: ColumnId::new(5),
                        value: ColumnValue::String("x"),
                    },
                    ColumnInput {
                        column: ColumnId::new(6),
                        value: ColumnValue::String("raw"),
                    },
                ],
            )
            .expect("row");
    }
    let columns = builder.finish().expect("columns");
    let alive = AliveSet::new(rows);
    let rescore = (0..rows as usize * dims as usize)
        .map(|index| index as f32 * 0.25 - 1.0)
        .collect::<Vec<_>>();
    let id = SegmentId::new(u64::from(dims), [rows as u8; 10]);
    let bytes = if int8 {
        let codes = vec![3_u8; rows as usize * dims as usize];
        let factors = vec![
            Int8Factors {
                scale: 0.25,
                offset: -1.0,
            };
            rows as usize
        ];
        encode_segment(SegmentBuild {
            id,
            scheme: 2,
            dims,
            codes: &codes,
            factors: SegmentFactors::Int8(&factors),
            rescore: &rescore,
            columns: &columns,
            alive: &alive,
        })
        .expect("Int8 segment")
    } else {
        let mut codes = Vec::new();
        let mut factors = Vec::new();
        for row in rescore.chunks_exact(dims as usize) {
            let mut encoded = vec![0_u8; (dims as usize).div_ceil(2)];
            factors.push(quantize_bit4(row, &mut encoded).expect("Bit4"));
            codes.extend_from_slice(&encoded);
        }
        encode_segment(SegmentBuild {
            id,
            scheme: 4,
            dims,
            codes: &codes,
            factors: SegmentFactors::Bit4(&factors),
            rescore: &rescore,
            columns: &columns,
            alive: &alive,
        })
        .expect("Bit4 segment")
    };
    (bytes, id)
}

#[test]
fn format_every_registered_family_and_edge_shape_matches_checked_in_golden() {
    let directory = tempfile::tempdir().expect("tempdir");
    let manifest = Manifest {
        generation: 3,
        log_seq: 2,
        segments: Vec::new(),
        epochs: vec![EpochMeta {
            id: 1,
            model: "m".to_owned(),
            tokenizer: "t".to_owned(),
        }],
        schema: Schema::new(Vec::new()).expect("schema"),
    };
    let manifest_bytes = encode_manifest(&manifest).expect("manifest");
    assert_eq!(
        manifest_bytes,
        fixture(include_str!("fixtures/format/manifest_v1.hex"))
    );
    assert_eq!(
        decode_manifest("manifest.golden", &manifest_bytes).expect("manifest decode"),
        manifest
    );

    let cases = [
        (
            "ZERO",
            3,
            0,
            false,
            include_str!("fixtures/format/segment_zero_header_v1.hex"),
        ),
        (
            "ONE",
            3,
            1,
            false,
            include_str!("fixtures/format/segment_one_header_v1.hex"),
        ),
        ("TAIL65", 65, 1, false, ""),
        ("INT8", 3, 1, true, ""),
    ];
    for (label, dims, rows, int8, header_fixture) in cases {
        let (bytes, id) = golden_segment(dims, rows, int8);
        let path = directory.path().join(format!("{label}.zseg"));
        std::fs::write(&path, &bytes).expect("write");
        let reader = SegmentReader::open(&path, id).expect("open");
        reader.validate_all().expect("all bytes");
        if !header_fixture.is_empty() {
            assert_eq!(
                &bytes[..reader.header_length()],
                fixture(header_fixture),
                "{label} segment header"
            );
        }
        match label {
            "ZERO" => {
                assert_eq!(reader.meta().row_count, 0);
                assert_eq!(reader.columns().expect("columns").row_count(), 0);
                assert_eq!(reader.alive().expect("alive").row_count(), 0);
                assert_eq!(
                    reader.region(RegionKind::Columns).expect("columns"),
                    fixture(include_str!("fixtures/format/columns_zero_v1.hex"))
                );
                assert_eq!(
                    reader.region(RegionKind::Alive).expect("alive"),
                    fixture(include_str!("fixtures/format/alive_zero_v1.hex"))
                );
                assert_eq!(
                    reader.region(RegionKind::ChecksumTable).expect("checksums"),
                    fixture(include_str!("fixtures/format/checksum_table_zero_v1.hex"))
                );
            }
            "ONE" => {
                assert_eq!(reader.meta().row_count, 1);
                assert_eq!(
                    reader.region(RegionKind::Columns).expect("columns"),
                    fixture(include_str!("fixtures/format/columns_one_v1.hex"))
                );
                assert_eq!(
                    reader.region(RegionKind::Alive).expect("alive"),
                    fixture(include_str!("fixtures/format/alive_one_v1.hex"))
                );
                assert_eq!(
                    reader.region(RegionKind::VectorCodes).expect("codes"),
                    fixture(include_str!("fixtures/format/vector_codes_odd_v1.hex"))
                );
                assert_eq!(
                    reader.region(RegionKind::VectorFactors).expect("factors"),
                    fixture(include_str!("fixtures/format/vector_factors_bit4_v1.hex"))
                );
                assert_eq!(
                    reader.region(RegionKind::VectorRescore).expect("rescore"),
                    fixture(include_str!("fixtures/format/vector_rescore_one_v1.hex"))
                );
                assert_eq!(reader.bit4_codes().expect("odd codes").last(), Some(&0x40));
            }
            "TAIL65" => {
                assert_eq!(reader.meta().dims, 65);
                assert_eq!(
                    reader.region(RegionKind::VectorCodes).expect("tail codes"),
                    fixture(include_str!("fixtures/format/vector_codes_tail65_v1.hex"))
                );
                assert_eq!(reader.bit4_codes().expect("tail codes").last(), Some(&0xf0));
            }
            "INT8" => {
                assert_eq!(std::mem::size_of::<Int8Factors>(), 8);
                assert_eq!(
                    reader
                        .region(RegionKind::VectorFactors)
                        .expect("Int8 factors"),
                    fixture(include_str!("fixtures/format/vector_factors_int8_v1.hex"))
                );
                assert_eq!(
                    reader.int8_factors().expect("Int8 factors"),
                    &[Int8Factors {
                        scale: 0.25,
                        offset: -1.0,
                    }]
                );
            }
            actual => panic!("unknown golden case {actual}"),
        }
    }
    assert_eq!(
        std::mem::size_of::<zeppelin_embed::quant::Bit4Factors>(),
        12
    );
    assert_eq!(
        fixture(include_str!("fixtures/format/postings_reserved_v1.hex")),
        Vec::<u8>::new()
    );
    assert_eq!(FormatRegistry::families().len(), 11);
    assert_eq!(FormatFamily::Wal.id(), 11);
    assert_eq!(
        FormatRegistry::require(FormatFamily::Wal.id(), 1)
            .expect("WAL family")
            .family,
        FormatFamily::Wal
    );
    assert_eq!(
        FormatRegistry::require_scheme(3),
        Err(RegistryError::RetiredScheme(3))
    );
    assert_eq!(
        FormatRegistry::require_scheme(5),
        Err(RegistryError::RetiredScheme(5))
    );

    let (mut unknown, id) = golden_segment(3, 0, false);
    unknown[192..194].copy_from_slice(&65_000_u16.to_le_bytes());
    let header_checksum = xxhash_rust::xxh3::xxh3_64(&unknown[..256]).to_le_bytes();
    unknown[256..264].copy_from_slice(&header_checksum);
    let trailer = unknown.len() - 8;
    let file_checksum = xxhash_rust::xxh3::xxh3_64(&unknown[..trailer]).to_le_bytes();
    unknown[trailer..].copy_from_slice(&file_checksum);
    assert_eq!(
        &unknown[..264],
        fixture(include_str!("fixtures/format/unknown_kind_header_v1.hex"))
    );
    let unknown_path = directory.path().join("UNKNOWN.zseg");
    std::fs::write(&unknown_path, &unknown).expect("unknown write");
    let reader = SegmentReader::open(&unknown_path, id).expect("unknown kind is skippable");
    assert_eq!(reader.unknown_region_ids(), vec![65_000]);
    reader
        .validate_all()
        .expect("unknown region bytes validate");
}
