#![allow(
    clippy::expect_used,
    clippy::indexing_slicing,
    clippy::panic,
    clippy::unwrap_used
)]

use proptest::prelude::*;
use proptest::test_runner::{Config, RngSeed, TestRunner};
use tempfile::tempdir;
use zeppelin_embed::lifecycle::durability::{CommitTier, DurabilityMode, DurabilityPolicy};
use zeppelin_embed::meta::{
    AliveSet, ColumnDefinition, ColumnId, ColumnInput, ColumnStore, ColumnStoreBuilder, ColumnType,
    ColumnValue, Schema,
};
use zeppelin_embed::quant::{Bit4Factors, est_dot_bit4_batch, prepare_bit4_query, quantize_bit4};
use zeppelin_embed::segment::SegmentId;
use zeppelin_embed::segment::layout::{RegionKind, VECTOR_HEADER_LEN};
use zeppelin_embed::segment::reader::SegmentReader;
use zeppelin_embed::segment::writer::{SegmentBuild, SegmentFactors, write_segment};
use zeppelin_embed::vfs::StdVfs;

fn ordered_policy() -> DurabilityPolicy {
    DurabilityPolicy::new(DurabilityMode::Durable, CommitTier::Ordered)
        .expect("ordered durability policy")
}

fn columns(rows: u32, salt: i16) -> ColumnStore {
    let schema = Schema::new(vec![
        ColumnDefinition::new(ColumnId::new(1), "value", ColumnType::U64, true),
        ColumnDefinition::new(ColumnId::new(2), "label", ColumnType::RawString, true),
    ])
    .expect("schema");
    let mut builder = ColumnStoreBuilder::new(schema);
    for row in 0..rows {
        let label = format!("r{row}-{salt}");
        let mut inputs = vec![ColumnInput {
            column: ColumnId::new(1),
            value: ColumnValue::U64(u64::from(row).saturating_add(salt.unsigned_abs().into())),
        }];
        if row % 2 == 0 {
            inputs.push(ColumnInput {
                column: ColumnId::new(2),
                value: ColumnValue::String(&label),
            });
        }
        builder
            .push_row(i64::from(salt) * 100 + i64::from(row), &inputs)
            .expect("row");
    }
    builder.finish().expect("columns")
}

#[test]
fn prop_segment_roundtrip() {
    let mut runner = TestRunner::new(Config {
        cases: 96,
        rng_seed: RngSeed::Fixed(0x70_07_5e_6d_65_6e_74),
        ..Config::default()
    });
    let corpus = (1_u32..=70, 0_u32..=4, any::<i16>()).prop_flat_map(|(dims, rows, salt)| {
        let value_count = dims as usize * rows as usize;
        (
            Just(dims),
            Just(rows),
            Just(salt),
            prop::collection::vec(-4.0_f32..4.0_f32, value_count),
        )
    });
    let result = runner.run(&corpus, |(dims, rows, salt, rescore)| {
        let row_bytes = (dims as usize).div_ceil(2);
        let mut codes = Vec::with_capacity(row_bytes * rows as usize);
        let mut factors = Vec::<Bit4Factors>::with_capacity(rows as usize);
        for row in rescore.chunks_exact(dims as usize) {
            let mut encoded = vec![0_u8; row_bytes];
            factors.push(quantize_bit4(row, &mut encoded).expect("finite row"));
            codes.extend_from_slice(&encoded);
        }
        let columns = columns(rows, salt);
        let mut alive = AliveSet::new(rows);
        for row in 0..rows {
            if row % 3 == 1 {
                alive.tombstone(row).expect("row exists");
            }
        }
        let id = SegmentId::new(0x0102_0304_0506, [salt as u8; 10]);
        let directory = tempdir().expect("tempdir");
        let meta = write_segment(
            &StdVfs,
            directory.path(),
            SegmentBuild {
                id,
                scheme: 4,
                dims,
                codes: &codes,
                factors: SegmentFactors::Bit4(&factors),
                rescore: &rescore,
                columns: &columns,
                alive: &alive,
            },
            ordered_policy(),
        )
        .expect("segment write");
        let reader =
            SegmentReader::open(&directory.path().join(id.file_name()), id).expect("segment open");
        reader.validate_all().expect("all bytes validate");

        let decoded_codes = reader.bit4_codes().expect("Bit4 codes");
        prop_assert_eq!(decoded_codes, codes.as_slice());
        prop_assert_eq!(reader.meta(), &meta);
        prop_assert_eq!(
            reader.bit4_factors().expect("Bit4 factors"),
            factors.as_slice()
        );
        prop_assert_eq!(reader.rescore_f32().expect("rescore"), rescore.as_slice());
        prop_assert_eq!(reader.columns().expect("columns"), columns);
        prop_assert_eq!(reader.alive().expect("alive"), alive);

        let raw_codes = reader.region(RegionKind::VectorCodes).expect("code region");
        let payload = raw_codes.get(VECTOR_HEADER_LEN..).expect("code payload");
        prop_assert_eq!(decoded_codes.as_ptr(), payload.as_ptr());
        prop_assert_eq!(
            decoded_codes.as_ptr(),
            reader.bit4_codes().expect("repeat").as_ptr()
        );
        prop_assert_eq!(
            reader.bit4_factors().expect("repeat factors").as_ptr(),
            reader.bit4_factors().expect("repeat factors").as_ptr()
        );
        if rows > 0 {
            let query =
                prepare_bit4_query(rescore.get(..dims as usize).expect("first row"), 0x7007)
                    .expect("query");
            let mut scores = vec![0.0_f32; rows as usize];
            est_dot_bit4_batch(
                &query,
                decoded_codes,
                reader.bit4_factors().expect("kernel factors"),
                &mut scores,
            )
            .expect("mmap-backed kernel input");
            prop_assert_eq!(scores.len(), rows as usize);
        }
        Ok(())
    });
    assert!(result.is_ok(), "property result: {result:?}");
}

#[test]
fn segment_large_regions_are_validated_lazily_per_64k_chunk() {
    let dims = 65_536_u32;
    let rows = 3_u32;
    let row_bytes = (dims as usize).div_ceil(2);
    let codes = vec![0_u8; row_bytes * rows as usize];
    let factors = vec![Bit4Factors::from_persisted(1.0, 1.0, 1.0); rows as usize];
    let rescore = vec![0.0_f32; dims as usize * rows as usize];
    let columns = columns(rows, 0);
    let alive = AliveSet::new(rows);
    let id = SegmentId::new(9, [9; 10]);
    let directory = tempdir().expect("tempdir");
    let path = directory.path().join(id.file_name());
    write_segment(
        &StdVfs,
        directory.path(),
        SegmentBuild {
            id,
            scheme: 4,
            dims,
            codes: &codes,
            factors: SegmentFactors::Bit4(&factors),
            rescore: &rescore,
            columns: &columns,
            alive: &alive,
        },
        ordered_policy(),
    )
    .expect("segment");
    let reader = SegmentReader::open(&path, id).expect("open");
    assert_eq!(
        reader
            .region_chunk(RegionKind::VectorCodes, 0)
            .expect("chunk 0")
            .len(),
        65_536
    );
    assert_eq!(
        reader
            .region_chunk(RegionKind::VectorCodes, 1)
            .expect("chunk 1")
            .len(),
        32_800
    );

    let entry = reader
        .directory()
        .iter()
        .find(|entry| entry.kind == RegionKind::VectorCodes.id())
        .copied()
        .expect("code entry");
    drop(reader);
    let mut bytes = std::fs::read(&path).expect("read segment");
    bytes[entry.offset as usize + 65_536] ^= 1;
    std::fs::write(&path, bytes).expect("write damage");
    let damaged = SegmentReader::open(&path, id).expect("header still valid");
    assert!(damaged.region_chunk(RegionKind::VectorCodes, 0).is_ok());
    let error = damaged
        .region_chunk(RegionKind::VectorCodes, 1)
        .expect_err("damaged chunk must fail independently");
    assert!(error.to_string().contains("chunk-1"), "{error}");
}
