#![allow(clippy::expect_used, clippy::indexing_slicing)]

use std::io::{Read, Write};
use std::path::{Path, PathBuf};

use tempfile::tempdir;
use xxhash_rust::xxh3::xxh3_64;
use zeppelin_embed::format::frame::{FILE_HEADER_LEN, FILE_TRAILER_LEN};
use zeppelin_embed::ingest::{
    DocId, DocumentVersion, IngestBatch, IngestDocument, Revision, RowSource, SearchRequest,
};
use zeppelin_embed::lifecycle::durability::{CommitTier, DurabilityMode, DurabilityPolicy};
use zeppelin_embed::lifecycle::{
    CancelToken, InMemorySegment, InMemorySegmentFactors, OpenOptions, QueryControl, Store,
    StoreError,
};
use zeppelin_embed::manifest::Manifest;
use zeppelin_embed::manifest::io::commit_manifest;
use zeppelin_embed::meta::{AliveSet, ColumnStore, ColumnStoreBuilder, Schema};
use zeppelin_embed::quant::{Bit4Factors, prepare_bit4_query, quantize_bit4};
use zeppelin_embed::scan::{ScanOptions, ScanQuery, ScanRequest, ScanRows, top_k};
use zeppelin_embed::segment::SegmentId;
use zeppelin_embed::segment::layout::{
    Int8Factors, REGION_ENTRY_LEN, RegionKind, SEGMENT_PREFIX_LEN, VECTOR_HEADER_LEN,
};
use zeppelin_embed::segment::reader::{SegmentReader, validate_segment_bytes};
use zeppelin_embed::segment::writer::{SegmentBuild, SegmentFactors, write_segment};
use zeppelin_embed::vfs::{StdVfs, SyncKind, Vfs, VfsFile};

const DIMS: usize = 32;
const ROWS: usize = 3;

struct Fixture {
    id: SegmentId,
    codes: Vec<u8>,
    factors: Vec<Bit4Factors>,
    rescore: Vec<f32>,
    columns: ColumnStore,
    alive: AliveSet,
}

impl Fixture {
    fn new(entropy: u8) -> Self {
        let query = fixture_query_values();
        let amplitudes = [-2.0_f32, 1.0, 4.0];
        let rotation = usize::from(entropy) % ROWS;
        let mut rescore = Vec::with_capacity(DIMS * ROWS);
        for row in 0..ROWS {
            let amplitude = amplitudes[(row + rotation) % ROWS];
            rescore.extend(query.iter().map(|value| value * amplitude));
        }
        let row_bytes = DIMS.div_ceil(2);
        let mut codes = vec![0_u8; row_bytes * ROWS];
        let mut factors = Vec::with_capacity(ROWS);
        for (row, encoded) in rescore
            .chunks_exact(DIMS)
            .zip(codes.chunks_exact_mut(row_bytes))
        {
            factors.push(quantize_bit4(row, encoded).expect("finite fixture row"));
        }
        let schema = Schema::new(Vec::new()).expect("timestamp-only schema");
        let mut builder = ColumnStoreBuilder::new(schema);
        for timestamp in 0_i64..ROWS as i64 {
            builder.push_row(timestamp, &[]).expect("fixture row");
        }
        Self {
            id: SegmentId::new(0x0102_0304_0506, [entropy; 10]),
            codes,
            factors,
            rescore,
            columns: builder.finish().expect("fixture columns"),
            alive: AliveSet::new(ROWS as u32),
        }
    }
}

fn fixture_query_values() -> Vec<f32> {
    (0..DIMS)
        .map(|column| match column % 4 {
            0 => -1.0,
            1 => -0.25,
            2 => 0.5,
            _ => 1.5,
        })
        .collect()
}

fn derived_policy() -> DurabilityPolicy {
    DurabilityPolicy::new(DurabilityMode::Derived, CommitTier::Ordered).expect("derived policy")
}

fn publish_fixture(directory: &std::path::Path, fixture: &Fixture, generation: u64) {
    publish_fixtures(directory, &[fixture], generation);
}

fn publish_fixtures(directory: &std::path::Path, fixtures: &[&Fixture], generation: u64) {
    let mut segments = Vec::with_capacity(fixtures.len());
    for fixture in fixtures {
        segments.push(
            write_segment(
                &StdVfs,
                directory,
                SegmentBuild {
                    id: fixture.id,
                    scheme: 4,
                    dims: DIMS as u32,
                    codes: &fixture.codes,
                    factors: SegmentFactors::Bit4(&fixture.factors),
                    rescore: &fixture.rescore,
                    columns: &fixture.columns,
                    alive: &fixture.alive,
                },
                derived_policy(),
            )
            .expect("fixture segment"),
        );
    }
    commit_manifest(
        &StdVfs,
        directory,
        &Manifest {
            generation,
            log_seq: 0,
            segments,
            epochs: Vec::new(),
            epoch_alias: None,
            schema: fixtures[0].columns.schema().clone(),
        },
        derived_policy(),
    )
    .expect("fixture manifest");
}

#[test]
fn sealing_drops_the_anonymous_copy_and_serves_from_the_mapping() {
    let directory = tempdir().expect("store directory");
    let original = Fixture::new(0x11);
    publish_fixture(directory.path(), &original, 1);
    let store = Store::open(directory.path(), OpenOptions::default()).expect("open mapped store");

    let replacement = Fixture::new(0x22);
    let query_values = fixture_query_values();
    let Fixture {
        id,
        codes,
        factors,
        rescore,
        columns,
        alive,
    } = replacement;
    let prepared = store
        .prepare_segment(InMemorySegment {
            id,
            scheme: 4,
            dims: DIMS as u32,
            codes,
            factors: InMemorySegmentFactors::Bit4(factors),
            rescore,
            columns: &columns,
            alive: &alive,
        })
        .expect("adopt in-memory segment");
    let sealed_size = prepared.resident_bytes();
    let before = store.stats().expect("stats before seal");

    let generation = store.seal_snapshot(prepared).expect("seal snapshot");

    let after = store.stats().expect("stats after seal");
    assert_eq!(generation, 2);
    assert!(
        before
            .resident_owned_bytes
            .saturating_sub(after.resident_owned_bytes)
            >= sealed_size,
        "anonymous bytes did not fall by the sealed size: before={} after={} sealed_size={sealed_size}",
        before.resident_owned_bytes,
        after.resident_owned_bytes
    );

    let lease = store.snapshot().expect("mapped snapshot lease");
    assert_eq!(lease.generation(), generation);
    assert_eq!(lease.segments().len(), 1, "seal publishes one segment");
    let segment = lease.segments().first().expect("sealed mapped segment");
    assert_eq!(segment.meta().id, id, "seal must preserve the prepared id");
    let query = prepare_bit4_query(&query_values, 0x09c0).expect("prepared query");
    let hits = top_k(
        ScanRequest {
            query: ScanQuery::Bit4(&query),
            rows: ScanRows::Bit4RowMajor {
                codes: segment.bit4_codes().expect("mmap-backed codes"),
                factors: segment.bit4_factors().expect("mmap-backed factors"),
            },
            row_mask: None,
        },
        3,
    )
    .expect("mmap-backed query");
    assert_eq!(hits.len(), 3);
    assert_eq!(
        hits.iter().map(|hit| hit.row_id).collect::<Vec<_>>(),
        vec![1, 0, 2],
        "replacement entropy must produce its generation-specific full ranking"
    );

    drop(lease);
    store.close().expect("close");
}

#[test]
fn seal_generation_is_strictly_after_ingest_generations() {
    let directory = tempdir().expect("store directory");
    let original = Fixture::new(0x31);
    publish_fixture(directory.path(), &original, 1);
    let store = Store::open(directory.path(), OpenOptions::default()).expect("open mapped store");
    let mut mutation_generations = Vec::new();
    for value in 0_u128..3 {
        let ack = store
            .ingest(IngestBatch::new(vec![IngestDocument::new(
                DocumentVersion::new(DocId::new(200 + value), Revision::new(1)),
                vec![1.0_f32; DIMS],
            )]))
            .expect("ingest before seal");
        mutation_generations.push(ack.generation());
    }

    let replacement = Fixture::new(0x32);
    let Fixture {
        id,
        codes,
        factors,
        rescore,
        columns,
        alive,
    } = replacement;
    let prepared = store
        .prepare_segment(InMemorySegment {
            id,
            scheme: 4,
            dims: DIMS as u32,
            codes,
            factors: InMemorySegmentFactors::Bit4(factors),
            rescore,
            columns: &columns,
            alive: &alive,
        })
        .expect("adopt replacement segment");

    let sealed_generation = store.seal_snapshot(prepared).expect("seal snapshot");

    assert!(
        mutation_generations
            .iter()
            .all(|generation| sealed_generation > *generation),
        "sealed generation {sealed_generation} did not follow {mutation_generations:?}"
    );
    assert_eq!(
        store.snapshot().expect("sealed snapshot").generation(),
        sealed_generation
    );
    store.close().expect("close");
}

#[test]
fn int8_buffers_use_the_same_accounted_seal_path() {
    let directory = tempdir().expect("store directory");
    let original = Fixture::new(0x66);
    publish_fixture(directory.path(), &original, 1);
    let store = Store::open(directory.path(), OpenOptions::default()).expect("open mapped store");
    let schema = Schema::new(Vec::new()).expect("timestamp-only schema");
    let mut builder = ColumnStoreBuilder::new(schema);
    for timestamp in 0_i64..ROWS as i64 {
        builder.push_row(timestamp, &[]).expect("fixture row");
    }
    let columns = builder.finish().expect("fixture columns");
    let alive = AliveSet::new(ROWS as u32);
    let codes = vec![1_u8; DIMS * ROWS];
    let factors = vec![
        Int8Factors {
            scale: 0.5,
            offset: -1.0,
        };
        ROWS
    ];
    let rescore = vec![0.25_f32; DIMS * ROWS];
    let expected_bytes = u64::try_from(
        codes.capacity()
            + factors.capacity() * std::mem::size_of::<Int8Factors>()
            + rescore.capacity() * std::mem::size_of::<f32>(),
    )
    .expect("fixture byte count");
    let prepared = store
        .prepare_segment(InMemorySegment {
            id: SegmentId::new(0x0102_0304_0506, [0x77; 10]),
            scheme: 2,
            dims: DIMS as u32,
            codes,
            factors: InMemorySegmentFactors::Int8(factors),
            rescore,
            columns: &columns,
            alive: &alive,
        })
        .expect("adopt Int8 buffers");
    assert_eq!(prepared.resident_bytes(), expected_bytes);

    store.seal_snapshot(prepared).expect("seal Int8 snapshot");

    let query = vec![1.0_f32; DIMS];
    let outcome = store
        .search(
            SearchRequest::new(&query),
            ROWS,
            ScanOptions { thread_budget: 1 },
            QueryControl::Cancel(CancelToken::new()),
        )
        .expect("search sealed Int8 snapshot through Store");
    assert_eq!(outcome.candidates.len(), ROWS);
    assert!(
        outcome
            .candidates
            .iter()
            .all(|candidate| candidate.row_id().source()
                == RowSource::Sealed(SegmentId::new(0x0102_0304_0506, [0x77; 10])))
    );

    let lease = store.snapshot().expect("Int8 snapshot lease");
    let segment = lease.segments().first().expect("Int8 segment");
    assert_eq!(segment.meta().scheme, 2);
    assert_eq!(
        segment.int8_factors().expect("mapped Int8 factors")[0].scale,
        0.5
    );
    assert_eq!(segment.rescore_f32().expect("mapped rescore")[0], 0.25);
    drop(lease);
    store.close().expect("close");
}

#[test]
fn int8_store_search_rejects_checksum_valid_wrong_stride() {
    let directory = tempdir().expect("store directory");
    let schema = Schema::new(Vec::new()).expect("timestamp-only schema");
    let mut builder = ColumnStoreBuilder::new(schema.clone());
    for timestamp in 0_i64..ROWS as i64 {
        builder.push_row(timestamp, &[]).expect("fixture row");
    }
    let columns = builder.finish().expect("fixture columns");
    let alive = AliveSet::new(ROWS as u32);
    let id = SegmentId::new(0x0102_0304_0506, [0x79; 10]);
    let codes = vec![1_u8; DIMS * ROWS];
    let factors = vec![
        Int8Factors {
            scale: 0.5,
            offset: -1.0,
        };
        ROWS
    ];
    let rescore = vec![0.25_f32; DIMS * ROWS];
    let meta = write_segment(
        &StdVfs,
        directory.path(),
        SegmentBuild {
            id,
            scheme: 2,
            dims: DIMS as u32,
            codes: &codes,
            factors: SegmentFactors::Int8(&factors),
            rescore: &rescore,
            columns: &columns,
            alive: &alive,
        },
        derived_policy(),
    )
    .expect("write Int8 fixture");
    rewrite_int8_stride_with_valid_checksums(
        &directory.path().join(id.file_name()),
        DIMS as u32 + 1,
    );
    commit_manifest(
        &StdVfs,
        directory.path(),
        &Manifest {
            generation: 1,
            log_seq: 0,
            segments: vec![meta],
            epochs: Vec::new(),
            epoch_alias: None,
            schema,
        },
        derived_policy(),
    )
    .expect("publish wrong-stride fixture");
    let store = Store::open(directory.path(), OpenOptions::default()).expect("open store");
    let query = vec![1.0_f32; DIMS];

    let error = store
        .search(
            SearchRequest::new(&query),
            ROWS,
            ScanOptions { thread_budget: 1 },
            QueryControl::Cancel(CancelToken::new()),
        )
        .expect_err("wrong Int8 stride must fail Store search");

    assert!(matches!(
        error,
        zeppelin_embed::lifecycle::QueryError::Store(StoreError::Segment(
            zeppelin_embed::segment::SegmentError::Geometry(ref detail)
        )) if detail == "Int8 code stride/length 33/96, expected 32/96"
    ));
    store.close().expect("close");
}

fn rewrite_int8_stride_with_valid_checksums(path: &Path, wrong_stride: u32) {
    let mut bytes = std::fs::read(path).expect("read Int8 segment");
    let header_length = usize::try_from(read_u64_at(&bytes, 16)).expect("header length fits");
    let region_count = usize::from(read_u16_at(&bytes, FILE_HEADER_LEN + 20));
    let directory_start = FILE_HEADER_LEN + SEGMENT_PREFIX_LEN;
    let codes_entry = find_region_entry(
        &bytes,
        directory_start,
        region_count,
        RegionKind::VectorCodes,
    );
    let checksum_entry = find_region_entry(
        &bytes,
        directory_start,
        region_count,
        RegionKind::ChecksumTable,
    );
    let codes_start = usize::try_from(read_u64_at(&bytes, codes_entry + 8)).expect("codes offset");
    let codes_length =
        usize::try_from(read_u64_at(&bytes, codes_entry + 16)).expect("codes length");
    bytes[codes_start + 8..codes_start + 12].copy_from_slice(&wrong_stride.to_le_bytes());
    let codes_checksum = xxh3_64(&bytes[codes_start..codes_start + codes_length]);
    bytes[codes_entry + 24..codes_entry + 32].copy_from_slice(&codes_checksum.to_le_bytes());

    let table_start =
        usize::try_from(read_u64_at(&bytes, checksum_entry + 8)).expect("table offset");
    let table_length =
        usize::try_from(read_u64_at(&bytes, checksum_entry + 16)).expect("table length");
    let table_count = usize::try_from(read_u32_at(&bytes, table_start)).expect("table count");
    let mut updated_chunk = false;
    for index in 0..table_count {
        let entry = table_start + 8 + index * 16;
        if read_u16_at(&bytes, entry) == RegionKind::VectorCodes.id()
            && read_u32_at(&bytes, entry + 4) == 0
        {
            bytes[entry + 8..entry + 16].copy_from_slice(&codes_checksum.to_le_bytes());
            updated_chunk = true;
        }
    }
    assert!(
        updated_chunk,
        "checksum table omitted VectorCodes chunk zero"
    );
    let table_checksum = xxh3_64(&bytes[table_start..table_start + table_length]);
    bytes[checksum_entry + 24..checksum_entry + 32].copy_from_slice(&table_checksum.to_le_bytes());

    let header_checksum = xxh3_64(&bytes[..header_length - 8]);
    bytes[header_length - 8..header_length].copy_from_slice(&header_checksum.to_le_bytes());
    let trailer = bytes.len() - FILE_TRAILER_LEN;
    let file_checksum = xxh3_64(&bytes[..trailer]);
    bytes[trailer..].copy_from_slice(&file_checksum.to_le_bytes());
    validate_segment_bytes(&bytes).expect("wrong-stride fixture remains fully checksummed");
    std::fs::write(path, bytes).expect("write wrong-stride segment");
}

fn find_region_entry(
    bytes: &[u8],
    directory_start: usize,
    region_count: usize,
    kind: RegionKind,
) -> usize {
    (0..region_count)
        .map(|index| directory_start + index * REGION_ENTRY_LEN)
        .find(|offset| read_u16_at(bytes, *offset) == kind.id())
        .expect("region directory entry")
}

fn read_u16_at(bytes: &[u8], offset: usize) -> u16 {
    u16::from_le_bytes(bytes[offset..offset + 2].try_into().expect("u16 bytes"))
}

fn read_u32_at(bytes: &[u8], offset: usize) -> u32 {
    u32::from_le_bytes(bytes[offset..offset + 4].try_into().expect("u32 bytes"))
}

fn read_u64_at(bytes: &[u8], offset: usize) -> u64 {
    u64::from_le_bytes(bytes[offset..offset + 8].try_into().expect("u64 bytes"))
}

#[test]
fn adopted_vector_charge_uses_capacity_not_length() {
    const CODE_CAPACITY: usize = 4_096;

    let directory = tempdir().expect("store directory");
    let store = Store::open(directory.path(), OpenOptions::default()).expect("writer open");
    let schema = Schema::new(Vec::new()).expect("timestamp-only schema");
    let columns = ColumnStoreBuilder::new(schema)
        .finish()
        .expect("empty columns");
    let alive = AliveSet::new(0);
    let mut codes = Vec::with_capacity(CODE_CAPACITY);
    codes.extend_from_slice(&[0x12, 0x34, 0x56]);
    assert_eq!(codes.len(), 3, "fixture length");
    assert_eq!(codes.capacity(), CODE_CAPACITY, "fixture capacity");

    let prepared = store
        .prepare_segment(InMemorySegment {
            id: SegmentId::new(0x0102_0304_0506, [0x78; 10]),
            scheme: 4,
            dims: DIMS as u32,
            codes,
            factors: InMemorySegmentFactors::Bit4(Vec::new()),
            rescore: Vec::new(),
            columns: &columns,
            alive: &alive,
        })
        .expect("adopt spare-capacity vector");

    assert_eq!(prepared.resident_bytes(), CODE_CAPACITY as u64);
    drop(prepared);
    assert_eq!(
        store
            .stats()
            .expect("released capacity charge")
            .resident_owned_bytes,
        0
    );
    store.close().expect("close");
}

#[test]
fn read_only_handle_rejects_prepared_segment_ownership() {
    let directory = tempdir().expect("store directory");
    let store = Store::open(directory.path(), OpenOptions::read_only()).expect("read-only open");
    let schema = Schema::new(Vec::new()).expect("timestamp-only schema");
    let columns = ColumnStoreBuilder::new(schema)
        .finish()
        .expect("empty columns");
    let alive = AliveSet::new(0);

    let result = store.prepare_segment(InMemorySegment {
        id: SegmentId::new(0x0102_0304_0506, [0x88; 10]),
        scheme: 4,
        dims: DIMS as u32,
        codes: Vec::new(),
        factors: InMemorySegmentFactors::Bit4(Vec::new()),
        rescore: Vec::new(),
        columns: &columns,
        alive: &alive,
    });

    assert!(matches!(result, Err(StoreError::ReadOnly)));
    assert_eq!(
        StoreError::ReadOnly.to_string(),
        "store handle is read-only"
    );
    store.close().expect("close");
}

#[test]
fn closed_handle_rejects_prepared_segment_ownership() {
    let directory = tempdir().expect("store directory");
    let store = Store::open(directory.path(), OpenOptions::default()).expect("writer open");
    store.close().expect("close");
    let schema = Schema::new(Vec::new()).expect("timestamp-only schema");
    let columns = ColumnStoreBuilder::new(schema)
        .finish()
        .expect("empty columns");
    let alive = AliveSet::new(0);

    let result = store.prepare_segment(InMemorySegment {
        id: SegmentId::new(0x0102_0304_0506, [0x89; 10]),
        scheme: 4,
        dims: DIMS as u32,
        codes: Vec::new(),
        factors: InMemorySegmentFactors::Bit4(Vec::new()),
        rescore: Vec::new(),
        columns: &columns,
        alive: &alive,
    });

    assert!(matches!(result, Err(StoreError::Closed)));
}

#[test]
fn read_only_handle_rejects_a_segment_prepared_by_a_writer() {
    let writer_directory = tempdir().expect("writer directory");
    let read_only_directory = tempdir().expect("read-only directory");
    let writer = Store::open(writer_directory.path(), OpenOptions::default()).expect("writer open");
    let read_only =
        Store::open(read_only_directory.path(), OpenOptions::read_only()).expect("read-only open");
    let fixture = Fixture::new(0xbb);
    let Fixture {
        id,
        codes,
        factors,
        rescore,
        columns,
        alive,
    } = fixture;
    let prepared = writer
        .prepare_segment(InMemorySegment {
            id,
            scheme: 4,
            dims: DIMS as u32,
            codes,
            factors: InMemorySegmentFactors::Bit4(factors),
            rescore,
            columns: &columns,
            alive: &alive,
        })
        .expect("writer adopts segment");

    let result = read_only.seal_snapshot(prepared);

    assert!(matches!(result, Err(StoreError::ReadOnly)));
    assert_eq!(
        writer
            .stats()
            .expect("writer accounting released")
            .resident_owned_bytes,
        0
    );
    read_only.close().expect("close read-only");
    writer.close().expect("close writer");
}

#[test]
fn writer_rejects_a_segment_accounted_to_another_store() {
    let first_directory = tempdir().expect("first writer directory");
    let second_directory = tempdir().expect("second writer directory");
    let first = Store::open(first_directory.path(), OpenOptions::default()).expect("first writer");
    let second =
        Store::open(second_directory.path(), OpenOptions::default()).expect("second writer");
    let fixture = Fixture::new(0xbc);
    let Fixture {
        id,
        codes,
        factors,
        rescore,
        columns,
        alive,
    } = fixture;
    let prepared = first
        .prepare_segment(InMemorySegment {
            id,
            scheme: 4,
            dims: DIMS as u32,
            codes,
            factors: InMemorySegmentFactors::Bit4(factors),
            rescore,
            columns: &columns,
            alive: &alive,
        })
        .expect("first writer adopts segment");

    let error = second
        .seal_snapshot(prepared)
        .expect_err("a different writer must reject foreign accounting");

    assert_eq!(
        error.to_string(),
        "prepared segment belongs to another store"
    );
    assert!(std::error::Error::source(&error).is_none());
    assert_eq!(
        first
            .stats()
            .expect("first writer accounting released")
            .resident_owned_bytes,
        0
    );
    second.close().expect("close second writer");
    first.close().expect("close first writer");
}

#[test]
fn seal_rejects_generation_overflow_before_writing() {
    let directory = tempdir().expect("store directory");
    let fixture = Fixture::new(0x99);
    publish_fixture(directory.path(), &fixture, u64::MAX);
    let store = Store::open(directory.path(), OpenOptions::default()).expect("open mapped store");
    let resident_before_prepare = store
        .stats()
        .expect("stats before prepare")
        .resident_owned_bytes;
    let replacement = Fixture::new(0xaa);
    let Fixture {
        id,
        codes,
        factors,
        rescore,
        columns,
        alive,
    } = replacement;
    let prepared = store
        .prepare_segment(InMemorySegment {
            id,
            scheme: 4,
            dims: DIMS as u32,
            codes,
            factors: InMemorySegmentFactors::Bit4(factors),
            rescore,
            columns: &columns,
            alive: &alive,
        })
        .expect("adopt replacement");
    let listing_before = directory_listing(directory.path());

    let result = store.seal_snapshot(prepared);

    assert!(matches!(result, Err(StoreError::GenerationOverflow)));
    assert_eq!(
        StoreError::GenerationOverflow.to_string(),
        "store snapshot generation overflow"
    );
    assert_eq!(
        store
            .stats()
            .expect("released preparation")
            .resident_owned_bytes,
        resident_before_prepare
    );
    assert_eq!(
        directory_listing(directory.path()),
        listing_before,
        "generation overflow must not create or replace any directory entry"
    );
    store.close().expect("close");
}

fn directory_listing(directory: &Path) -> Vec<std::ffi::OsString> {
    let mut entries = std::fs::read_dir(directory)
        .expect("read store directory")
        .map(|entry| entry.map(|value| value.file_name()))
        .collect::<Result<Vec<_>, _>>()
        .expect("collect store directory");
    entries.sort();
    entries
}

#[test]
fn durable_state_is_never_mapped_writable() {
    let directory = tempdir().expect("store directory");
    let first = Fixture::new(0x33);
    let second = Fixture::new(0x34);
    publish_fixtures(directory.path(), &[&first, &second], 1);
    let store = Store::open(directory.path(), OpenOptions::default()).expect("open mapped store");
    let lease = store.snapshot().expect("snapshot lease");
    assert_eq!(
        lease.segments().len(),
        2,
        "fixture must publish two segments"
    );

    for segment in lease.segments() {
        let mapped_address = segment.bit4_codes().expect("mapped codes").as_ptr() as usize;
        assert!(
            kernel_mapping_is_read_only(mapped_address).expect("kernel mapping protection"),
            "kernel reports a writable mapping for segment {}",
            segment.meta().id
        );
    }

    drop(lease);
    store.close().expect("close");
}

#[test]
fn mapped_resident_bytes_tracks_full_touch_across_chunked_mapping() {
    const LARGE_DIMS: usize = 4_194_304;
    const MINCORE_STATUS_CAPACITY: u64 = 1_024;

    let directory = tempdir().expect("store directory");
    let schema = Schema::new(Vec::new()).expect("timestamp-only schema");
    let mut builder = ColumnStoreBuilder::new(schema.clone());
    builder.push_row(17, &[]).expect("fixture row");
    let columns = builder.finish().expect("fixture columns");
    let alive = AliveSet::new(1);
    let id = SegmentId::new(0x0102_0304_0506, [0x44; 10]);
    let codes = vec![0x88_u8; LARGE_DIMS.div_ceil(2)];
    let factors = [Bit4Factors::from_persisted(1.0, 1.0, 1.0)];
    let rescore = vec![0.0_f32; LARGE_DIMS];
    let segment = write_segment(
        &NoCacheVfs,
        directory.path(),
        SegmentBuild {
            id,
            scheme: 4,
            dims: LARGE_DIMS as u32,
            codes: &codes,
            factors: SegmentFactors::Bit4(&factors),
            rescore: &rescore,
            columns: &columns,
            alive: &alive,
        },
        derived_policy(),
    )
    .expect("large fixture segment");
    commit_manifest(
        &StdVfs,
        directory.path(),
        &Manifest {
            generation: 1,
            log_seq: 0,
            segments: vec![segment],
            epochs: Vec::new(),
            epoch_alias: None,
            schema,
        },
        derived_policy(),
    )
    .expect("large fixture manifest");
    drop((codes, rescore, columns, alive));
    let store = Store::open(directory.path(), OpenOptions::default()).expect("open mapped store");
    let lease = store.snapshot().expect("snapshot lease");
    let segment = &lease.segments()[0];
    let page_size = page_size().expect("system page size") as u64;
    let mapped_pages = segment.meta().file_size.div_ceil(page_size);
    assert!(
        mapped_pages > MINCORE_STATUS_CAPACITY,
        "fixture must enter the chunked mincore path: pages={mapped_pages} capacity={MINCORE_STATUS_CAPACITY}"
    );
    let before = store.stats().expect("residency before touching mapping");
    assert_eq!(before.mapped_bytes, segment.meta().file_size);
    assert!(
        before.mapped_resident_bytes <= before.mapped_bytes / 10,
        "no-cache fixture must begin at no more than 10% resident: resident={} mapped={}",
        before.mapped_resident_bytes,
        before.mapped_bytes
    );

    touch_every_mapping_page(segment).expect("touch every mapped page");

    let after = store.stats().expect("residency after touching mapping");
    assert!(
        after.mapped_resident_bytes >= after.mapped_bytes.saturating_mul(9) / 10,
        "touching every page must make at least 90% resident: resident={} mapped={}",
        after.mapped_resident_bytes,
        after.mapped_bytes
    );
    let resident_rise = after
        .mapped_resident_bytes
        .checked_sub(before.mapped_resident_bytes)
        .expect("touching must not reduce residency");
    assert!(
        resident_rise >= after.mapped_bytes.saturating_mul(8) / 10,
        "mincore residency must rise by at least 80% of the same mapping: before={} after={} rise={resident_rise} mapped={}",
        before.mapped_resident_bytes,
        after.mapped_resident_bytes,
        after.mapped_bytes
    );

    drop(lease);
    store.close().expect("close");
}

fn touch_every_mapping_page(segment: &SegmentReader) -> std::io::Result<()> {
    let page_size = page_size()?;
    let codes = segment.bit4_codes().map_err(std::io::Error::other)?;
    let code_entry = segment
        .directory()
        .iter()
        .find(|entry| entry.kind == RegionKind::VectorCodes.id())
        .ok_or_else(|| std::io::Error::other("mapped segment has no vector-code region"))?;
    let code_offset = usize::try_from(code_entry.offset)
        .ok()
        .and_then(|offset| offset.checked_add(VECTOR_HEADER_LEN))
        .ok_or_else(|| std::io::Error::other("vector-code mapping offset exceeds usize"))?;
    let mapping_length = usize::try_from(segment.meta().file_size)
        .map_err(|_| std::io::Error::other("mapped segment length exceeds usize"))?;
    let code_end = code_offset
        .checked_add(codes.len())
        .ok_or_else(|| std::io::Error::other("vector-code mapping range overflow"))?;
    if code_end > mapping_length {
        return Err(std::io::Error::other(
            "vector-code mapping range exceeds mapped file",
        ));
    }
    let mapping_start = unsafe {
        // SAFETY: `codes` is the validated mmap-backed payload beginning
        // exactly `entry.offset + VECTOR_HEADER_LEN` bytes into this live
        // segment mapping; the checked offset therefore reaches its base.
        codes.as_ptr().sub(code_offset)
    };
    for offset in (0..mapping_length).step_by(page_size) {
        let _ = unsafe {
            // SAFETY: `offset` is strictly below the validated mapped file
            // length, and the `SnapshotLease` keeps that read-only mapping live.
            std::ptr::read_volatile(mapping_start.add(offset))
        };
    }
    Ok(())
}

fn page_size() -> std::io::Result<usize> {
    let page_size = unsafe {
        // SAFETY: `_SC_PAGESIZE` takes no pointer arguments and has no preconditions.
        libc::sysconf(libc::_SC_PAGESIZE)
    };
    usize::try_from(page_size)
        .ok()
        .filter(|size| *size != 0)
        .ok_or_else(|| std::io::Error::other("sysconf returned an invalid page size"))
}

struct NoCacheVfs;

impl Vfs for NoCacheVfs {
    fn open(&self, path: &Path) -> std::io::Result<u64> {
        StdVfs.open(path)
    }

    fn open_for_map(&self, path: &Path) -> std::io::Result<std::fs::File> {
        let file = std::fs::File::open(path)?;
        disable_file_cache(&file)?;
        Ok(file)
    }

    fn read(&self, path: &Path) -> std::io::Result<Vec<u8>> {
        let mut file = std::fs::File::open(path)?;
        disable_file_cache(&file)?;
        let length = usize::try_from(file.metadata()?.len()).map_err(|_| {
            std::io::Error::new(std::io::ErrorKind::InvalidData, "file length exceeds usize")
        })?;
        let mut bytes = Vec::with_capacity(length);
        file.read_to_end(&mut bytes)?;
        Ok(bytes)
    }

    fn read_range(&self, path: &Path, offset: u64, length: usize) -> std::io::Result<Vec<u8>> {
        StdVfs.read_range(path, offset, length)
    }

    fn write(&self, path: &Path, bytes: &[u8]) -> std::io::Result<()> {
        let mut file = std::fs::OpenOptions::new()
            .create(true)
            .truncate(true)
            .write(true)
            .open(path)?;
        disable_file_cache(&file)?;
        file.write_all(bytes)?;
        drop(file);
        Ok(())
    }

    fn open_append(&self, path: &Path) -> std::io::Result<Box<dyn VfsFile>> {
        StdVfs.open_append(path)
    }

    fn rename(&self, from: &Path, to: &Path) -> std::io::Result<()> {
        StdVfs.rename(from, to)
    }

    fn sync(&self, path: &Path, kind: SyncKind) -> std::io::Result<()> {
        StdVfs.sync(path, kind)
    }

    fn list(&self, directory: &Path) -> std::io::Result<Vec<PathBuf>> {
        StdVfs.list(directory)
    }

    fn delete(&self, path: &Path) -> std::io::Result<()> {
        StdVfs.delete(path)
    }
}

#[cfg(target_os = "macos")]
fn disable_file_cache(file: &std::fs::File) -> std::io::Result<()> {
    use std::os::fd::AsRawFd;

    let result = unsafe {
        // SAFETY: `file` owns a live descriptor and `F_NOCACHE` takes one
        // integer boolean argument.
        libc::fcntl(file.as_raw_fd(), libc::F_NOCACHE, 1)
    };
    if result == -1 {
        Err(std::io::Error::last_os_error())
    } else {
        Ok(())
    }
}

#[cfg(target_os = "linux")]
fn disable_file_cache(file: &std::fs::File) -> std::io::Result<()> {
    use std::os::fd::AsRawFd;

    file.sync_data()?;
    let result = unsafe {
        // SAFETY: `file` owns a live descriptor and the zero length advises
        // through end-of-file without exposing a pointer.
        libc::posix_fadvise(file.as_raw_fd(), 0, 0, libc::POSIX_FADV_DONTNEED)
    };
    if result == 0 {
        Ok(())
    } else {
        Err(std::io::Error::from_raw_os_error(result))
    }
}

#[cfg(target_os = "macos")]
fn kernel_mapping_is_read_only(address: usize) -> std::io::Result<bool> {
    const VM_REGION_BASIC_INFO_64: libc::c_int = 9;
    const VM_REGION_BASIC_INFO_COUNT_64: libc::mach_msg_type_number_t = 9;
    const VM_PROT_WRITE: libc::vm_prot_t = 0x02;
    #[repr(C, packed(4))]
    #[derive(Default)]
    struct VmRegionBasicInfo64 {
        protection: libc::vm_prot_t,
        max_protection: libc::vm_prot_t,
        inheritance: libc::vm_inherit_t,
        shared: libc::boolean_t,
        reserved: libc::boolean_t,
        offset: libc::memory_object_offset_t,
        behavior: libc::c_int,
        user_wired_count: libc::c_ushort,
    }
    unsafe extern "C" {
        static mach_task_self_: libc::mach_port_t;
        fn mach_vm_region(
            target_task: libc::vm_map_t,
            address: *mut libc::mach_vm_address_t,
            size: *mut libc::mach_vm_size_t,
            flavor: libc::c_int,
            info: *mut libc::c_int,
            info_count: *mut libc::mach_msg_type_number_t,
            object_name: *mut libc::mach_port_t,
        ) -> libc::kern_return_t;
        fn mach_port_deallocate(
            task: libc::mach_port_t,
            name: libc::mach_port_t,
        ) -> libc::kern_return_t;
    }

    if std::mem::size_of::<VmRegionBasicInfo64>() != 36
        || std::mem::offset_of!(VmRegionBasicInfo64, offset) != 20
        || std::mem::offset_of!(VmRegionBasicInfo64, behavior) != 28
        || std::mem::offset_of!(VmRegionBasicInfo64, user_wired_count) != 32
    {
        return Err(std::io::Error::other(
            "vm_region_basic_info_64 Rust layout does not match Darwin pack(4)",
        ));
    }
    let target = u64::try_from(address)
        .map_err(|_| std::io::Error::other("mapping address exceeds mach_vm_address_t"))?;
    let mut region_address = target;
    let mut region_size = 0_u64;
    let mut info = VmRegionBasicInfo64::default();
    let mut info_count = VM_REGION_BASIC_INFO_COUNT_64;
    let mut object_name = 0;
    let task = unsafe {
        // SAFETY: libSystem initializes the current-task port before Rust `main`.
        mach_task_self_
    };
    let result = unsafe {
        // SAFETY: every out pointer refers to writable storage matching Darwin's
        // pack(4), 36-byte `vm_region_basic_info_64` layout and count 9.
        mach_vm_region(
            task,
            &raw mut region_address,
            &raw mut region_size,
            VM_REGION_BASIC_INFO_64,
            (&raw mut info).cast::<libc::c_int>(),
            &raw mut info_count,
            &raw mut object_name,
        )
    };
    if object_name != 0 {
        let _ = unsafe {
            // SAFETY: `mach_vm_region` returned this send right to the current task.
            mach_port_deallocate(task, object_name)
        };
    }
    if result != libc::KERN_SUCCESS {
        return Err(std::io::Error::other(format!(
            "mach_vm_region failed with Mach code {result}"
        )));
    }
    if !(region_address <= target && target < region_address.saturating_add(region_size)) {
        return Err(std::io::Error::other(
            "mach_vm_region returned a region after the target address",
        ));
    }
    Ok(info.protection & VM_PROT_WRITE == 0)
}

#[cfg(target_os = "linux")]
fn kernel_mapping_is_read_only(address: usize) -> std::io::Result<bool> {
    let maps = std::fs::read_to_string("/proc/self/maps")?;
    for line in maps.lines() {
        let mut fields = line.split_whitespace();
        let Some(range) = fields.next() else {
            continue;
        };
        let Some(protection) = fields.next() else {
            continue;
        };
        let Some((start, end)) = range.split_once('-') else {
            continue;
        };
        let start = usize::from_str_radix(start, 16).map_err(std::io::Error::other)?;
        let end = usize::from_str_radix(end, 16).map_err(std::io::Error::other)?;
        if start <= address && address < end {
            return Ok(!protection
                .as_bytes()
                .get(1)
                .is_some_and(|byte| *byte == b'w'));
        }
    }
    Err(std::io::Error::other(
        "target address is absent from /proc/self/maps",
    ))
}
