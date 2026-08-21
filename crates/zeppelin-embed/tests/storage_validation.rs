#![allow(
    clippy::expect_used,
    clippy::indexing_slicing,
    clippy::panic,
    clippy::type_complexity,
    clippy::unwrap_used
)]

use std::path::Path;
use std::sync::atomic::{AtomicUsize, Ordering};

use tempfile::tempdir;
use xxhash_rust::xxh3::xxh3_64;
use zeppelin_embed::lifecycle::durability::{CommitTier, DurabilityMode, DurabilityPolicy};
use zeppelin_embed::meta::{AliveSet, ColumnStore, ColumnStoreBuilder, Schema};
use zeppelin_embed::quant::Bit4Factors;
use zeppelin_embed::segment::layout::{Int8Factors, RegionKind};
use zeppelin_embed::segment::reader::SegmentReader;
use zeppelin_embed::segment::writer::{
    SegmentBuild, SegmentFactors, encode_segment, write_segment,
};
use zeppelin_embed::segment::{SegmentError, SegmentId};
use zeppelin_embed::vfs::{CountingVfs, StdVfs, SyncKind, Vfs, VfsFile};

fn ordered_policy() -> DurabilityPolicy {
    DurabilityPolicy::new(DurabilityMode::Durable, CommitTier::Ordered)
        .expect("ordered durability policy")
}

fn empty_columns() -> ColumnStore {
    ColumnStoreBuilder::new(Schema::new(Vec::new()).expect("schema"))
        .finish()
        .expect("columns")
}

fn one_row_columns() -> ColumnStore {
    let mut builder = ColumnStoreBuilder::new(Schema::new(Vec::new()).expect("schema"));
    builder.push_row(7, &[]).expect("row");
    builder.finish().expect("columns")
}

fn one_row_segment() -> (Vec<u8>, SegmentId) {
    let columns = one_row_columns();
    let alive = AliveSet::new(1);
    let factors = [Bit4Factors::from_persisted(1.0, 1.0, 1.0)];
    let id = SegmentId::new(5, [5; 10]);
    let bytes = encode_segment(SegmentBuild {
        id,
        scheme: 4,
        dims: 3,
        codes: &[0x10, 0x20],
        factors: SegmentFactors::Bit4(&factors),
        rescore: &[1.0, 2.0, 3.0],
        columns: &columns,
        alive: &alive,
    })
    .expect("segment");
    (bytes, id)
}

fn rewrite_header_and_file(bytes: &mut [u8]) {
    let header_length = u64::from_le_bytes(bytes[16..24].try_into().unwrap()) as usize;
    let header_checksum = xxh3_64(&bytes[..header_length - 8]).to_le_bytes();
    bytes[header_length - 8..header_length].copy_from_slice(&header_checksum);
    let trailer = bytes.len() - 8;
    let file_checksum = xxh3_64(&bytes[..trailer]).to_le_bytes();
    bytes[trailer..].copy_from_slice(&file_checksum);
}

fn rewrite_region(bytes: &mut [u8], entry_index: usize) {
    let directory = 64 + entry_index * 32;
    let offset =
        u64::from_le_bytes(bytes[directory + 8..directory + 16].try_into().unwrap()) as usize;
    let length =
        u64::from_le_bytes(bytes[directory + 16..directory + 24].try_into().unwrap()) as usize;
    let checksum = xxh3_64(&bytes[offset..offset + length]).to_le_bytes();
    bytes[directory + 24..directory + 32].copy_from_slice(&checksum);
    rewrite_header_and_file(bytes);
}

fn open_error(path: &Path, id: SegmentId) -> SegmentError {
    match SegmentReader::open(path, id) {
        Ok(_) => panic!("damaged header unexpectedly opened"),
        Err(error) => error,
    }
}

#[test]
fn std_and_counting_vfs_cover_the_complete_synchronous_seam() {
    let directory = tempdir().expect("tempdir");
    let source = directory.path().join("source");
    let renamed = directory.path().join("renamed");
    let counting = CountingVfs::new(StdVfs);
    counting.write(&source, b"abcdef").expect("write");
    assert_eq!(counting.open(&source).expect("open"), 6);
    assert_eq!(counting.read(&source).expect("read"), b"abcdef");
    assert_eq!(counting.read_range(&source, 2, 8).expect("range"), b"cdef");
    counting
        .sync(&source, SyncKind::Barrier)
        .expect("file sync");
    assert!(
        counting
            .list(directory.path())
            .expect("list")
            .contains(&source)
    );
    counting.rename(&source, &renamed).expect("rename");
    assert_eq!(counting.inner().open(&renamed).expect("inner"), 6);
    assert_eq!(counting.open_calls(), 1);
    assert_eq!(counting.read_calls(), 2);
    assert_eq!(counting.read_bytes(), 10);
    counting.reset();
    assert_eq!(
        (
            counting.open_calls(),
            counting.read_calls(),
            counting.read_bytes()
        ),
        (0, 0, 0)
    );
    counting.delete(&renamed).expect("delete");
    assert!(counting.open(&renamed).is_err());
    assert!(counting.read(&renamed).is_err());
    assert!(counting.read_range(&renamed, 0, 1).is_err());
    assert!(counting.rename(&renamed, &source).is_err());
    assert!(counting.delete(&renamed).is_err());
}

#[test]
fn segment_writer_rejects_every_cross_region_shape_before_writing() {
    let zero = empty_columns();
    let one = one_row_columns();
    let alive_zero = AliveSet::new(0);
    let alive_one = AliveSet::new(1);
    let bit4 = [Bit4Factors::from_persisted(1.0, 1.0, 1.0)];
    let int8 = [Int8Factors {
        scale: 1.0,
        offset: 0.0,
    }];
    let id = SegmentId::new(1, [1; 10]);

    let errors = [
        encode_segment(SegmentBuild {
            id,
            scheme: 4,
            dims: 3,
            codes: &[],
            factors: SegmentFactors::Bit4(&[]),
            rescore: &[],
            columns: &zero,
            alive: &alive_one,
        }),
        encode_segment(SegmentBuild {
            id,
            scheme: 3,
            dims: 3,
            codes: &[],
            factors: SegmentFactors::Bit4(&[]),
            rescore: &[],
            columns: &zero,
            alive: &alive_zero,
        }),
        encode_segment(SegmentBuild {
            id,
            scheme: 2,
            dims: 3,
            codes: &[0; 3],
            factors: SegmentFactors::Bit4(&bit4),
            rescore: &[0.0; 3],
            columns: &one,
            alive: &alive_one,
        }),
        encode_segment(SegmentBuild {
            id,
            scheme: 4,
            dims: 3,
            codes: &[0; 2],
            factors: SegmentFactors::Bit4(&[]),
            rescore: &[0.0; 3],
            columns: &one,
            alive: &alive_one,
        }),
        encode_segment(SegmentBuild {
            id,
            scheme: 4,
            dims: 3,
            codes: &[0; 1],
            factors: SegmentFactors::Bit4(&bit4),
            rescore: &[0.0; 3],
            columns: &one,
            alive: &alive_one,
        }),
        encode_segment(SegmentBuild {
            id,
            scheme: 4,
            dims: 3,
            codes: &[0; 2],
            factors: SegmentFactors::Bit4(&bit4),
            rescore: &[0.0; 2],
            columns: &one,
            alive: &alive_one,
        }),
        encode_segment(SegmentBuild {
            id,
            scheme: 4,
            dims: 3,
            codes: &[0; 2],
            factors: SegmentFactors::Int8(&int8),
            rescore: &[0.0; 3],
            columns: &one,
            alive: &alive_one,
        }),
        encode_segment(SegmentBuild {
            id,
            scheme: 2,
            dims: 3,
            codes: &[0; 3],
            factors: SegmentFactors::Int8(&[]),
            rescore: &[0.0; 3],
            columns: &one,
            alive: &alive_one,
        }),
    ];
    for error in errors {
        let error = error.expect_err("shape must fail");
        assert!(error.to_string().contains("segment geometry"), "{error}");
    }
    assert!(id.file_name().starts_with("segment-"));
    assert_eq!(id.to_string().len(), 32);
}

#[test]
fn segment_header_and_vector_geometry_reject_specific_malformed_fields() {
    let directory = tempdir().expect("tempdir");
    let path = directory.path().join("damaged.zseg");
    let (valid, id) = one_row_segment();
    let header_cases: Vec<Box<dyn Fn(&mut [u8])>> = vec![
        Box::new(|bytes| bytes[10..12].copy_from_slice(&0_u16.to_le_bytes())),
        Box::new(|bytes| bytes[54..56].copy_from_slice(&1_u16.to_le_bytes())),
        Box::new(|bytes| bytes[56..58].copy_from_slice(&3_u16.to_le_bytes())),
        Box::new(|bytes| bytes[58..60].copy_from_slice(&1_u16.to_le_bytes())),
        Box::new(|bytes| bytes[66..68].copy_from_slice(&0_u16.to_le_bytes())),
        Box::new(|bytes| bytes[68..72].copy_from_slice(&1_u32.to_le_bytes())),
        Box::new(|bytes| bytes[96..98].copy_from_slice(&1_u16.to_le_bytes())),
        Box::new(|bytes| bytes[72..80].copy_from_slice(&65_u64.to_le_bytes())),
        Box::new(|bytes| bytes[104..112].copy_from_slice(&16_384_u64.to_le_bytes())),
        Box::new(|bytes| bytes[80..88].copy_from_slice(&u64::MAX.to_le_bytes())),
        Box::new(|bytes| bytes[52..54].copy_from_slice(&5_u16.to_le_bytes())),
    ];
    for mutate in header_cases {
        let mut bytes = valid.clone();
        mutate(&mut bytes);
        rewrite_header_and_file(&mut bytes);
        std::fs::write(&path, bytes).expect("write");
        assert!(!open_error(&path, id).to_string().is_empty());
    }

    let vector_cases = [
        (3_usize, 2_usize, 1_u64),
        (3, 14, 1),
        (3, 16, 1),
        (3, 28, 1),
        (3, 8, 1),
        (3, 4, 4),
        (2, 12, 8),
        (4, 0, 1),
    ];
    for (entry_index, field_offset, value) in vector_cases {
        let mut bytes = valid.clone();
        let directory = 64 + entry_index * 32;
        let region =
            u64::from_le_bytes(bytes[directory + 8..directory + 16].try_into().unwrap()) as usize;
        match field_offset {
            2 | 12 | 14 => bytes[region + field_offset..region + field_offset + 2]
                .copy_from_slice(&(value as u16).to_le_bytes()),
            4 | 8 | 28 => bytes[region + field_offset..region + field_offset + 4]
                .copy_from_slice(&(value as u32).to_le_bytes()),
            16 => bytes[region + field_offset..region + field_offset + 8]
                .copy_from_slice(&value.to_le_bytes()),
            0 => bytes[region..region + 2].copy_from_slice(&(value as u16).to_le_bytes()),
            _ => panic!("unexpected field"),
        }
        rewrite_region(&mut bytes, entry_index);
        std::fs::write(&path, bytes).expect("write");
        let reader = SegmentReader::open(&path, id).expect("header");
        let result = match entry_index {
            2 => reader.bit4_factors().map(|_| ()),
            3 => reader.bit4_codes().map(|_| ()),
            4 => reader.rescore_f32().map(|_| ()),
            _ => unreachable!(),
        };
        assert!(result.is_err());
    }

    std::fs::write(&path, &valid).expect("valid write");
    let reader = SegmentReader::open(&path, id).expect("valid open");
    assert!(reader.int8_factors().is_err());
    assert!(reader.region(RegionKind::Postings).is_err());
    assert!(reader.region_chunk(RegionKind::VectorCodes, 99).is_err());
}

#[test]
fn alive_and_column_decoders_reject_semantically_invalid_checked_bytes() {
    let directory = tempdir().expect("tempdir");
    let path = directory.path().join("semantic.zseg");
    let (valid, id) = one_row_segment();

    let mut alive_length = valid.clone();
    let alive_directory = 64 + 32;
    let alive_offset = u64::from_le_bytes(
        alive_length[alive_directory + 8..alive_directory + 16]
            .try_into()
            .unwrap(),
    ) as usize;
    alive_length[alive_offset + 4..alive_offset + 8].copy_from_slice(&2_u32.to_le_bytes());
    rewrite_region(&mut alive_length, 1);
    std::fs::write(&path, alive_length).expect("write");
    assert!(
        SegmentReader::open(&path, id)
            .expect("open")
            .alive()
            .is_err()
    );

    let mut alive_tail = valid.clone();
    alive_tail[alive_offset + 8] = 0x81;
    rewrite_region(&mut alive_tail, 1);
    std::fs::write(&path, alive_tail).expect("write");
    assert!(
        SegmentReader::open(&path, id)
            .expect("open")
            .alive()
            .is_err()
    );

    let columns_directory = 64;
    let columns_offset = u64::from_le_bytes(
        valid[columns_directory + 8..columns_directory + 16]
            .try_into()
            .unwrap(),
    ) as usize;
    let mut no_columns = valid.clone();
    no_columns[columns_offset + 4..columns_offset + 8].copy_from_slice(&0_u32.to_le_bytes());
    rewrite_region(&mut no_columns, 0);
    std::fs::write(&path, no_columns).expect("write");
    assert!(
        SegmentReader::open(&path, id)
            .expect("open")
            .columns()
            .is_err()
    );

    let mut bad_timestamp = valid.clone();
    bad_timestamp[columns_offset + 14..columns_offset + 16].copy_from_slice(&1_u16.to_le_bytes());
    rewrite_region(&mut bad_timestamp, 0);
    std::fs::write(&path, bad_timestamp).expect("write");
    assert!(
        SegmentReader::open(&path, id)
            .expect("open")
            .columns()
            .is_err()
    );
}

#[test]
fn segment_writer_surfaces_each_vfs_commit_stage() {
    struct FailingVfs;
    impl Vfs for FailingVfs {
        fn open(&self, _: &Path) -> std::io::Result<u64> {
            Err(std::io::ErrorKind::Other.into())
        }
        fn read(&self, _: &Path) -> std::io::Result<Vec<u8>> {
            Err(std::io::ErrorKind::Other.into())
        }
        fn read_range(&self, _: &Path, _: u64, _: usize) -> std::io::Result<Vec<u8>> {
            Err(std::io::ErrorKind::Other.into())
        }
        fn write(&self, _: &Path, _: &[u8]) -> std::io::Result<()> {
            Err(std::io::Error::other("write stage"))
        }
        fn open_append(&self, _: &Path) -> std::io::Result<Box<dyn VfsFile>> {
            Err(std::io::Error::other("append stage"))
        }
        fn rename(&self, _: &Path, _: &Path) -> std::io::Result<()> {
            Err(std::io::ErrorKind::Other.into())
        }
        fn sync(&self, _: &Path, _: SyncKind) -> std::io::Result<()> {
            Err(std::io::ErrorKind::Other.into())
        }
        fn list(&self, _: &Path) -> std::io::Result<Vec<std::path::PathBuf>> {
            Err(std::io::ErrorKind::Other.into())
        }
        fn delete(&self, _: &Path) -> std::io::Result<()> {
            Err(std::io::ErrorKind::Other.into())
        }
    }
    let columns = empty_columns();
    let alive = AliveSet::new(0);
    let directory = tempdir().expect("tempdir");
    let error = write_segment(
        &FailingVfs,
        directory.path(),
        SegmentBuild {
            id: SegmentId::new(0, [0; 10]),
            scheme: 4,
            dims: 3,
            codes: &[],
            factors: SegmentFactors::Bit4(&[]),
            rescore: &[],
            columns: &columns,
            alive: &alive,
        },
        ordered_policy(),
    )
    .expect_err("write stage");
    assert!(error.to_string().contains("write stage"));
}

struct StageVfs {
    inner: StdVfs,
    fail_sync_call: usize,
    fail_rename: bool,
    sync_calls: AtomicUsize,
}

impl Vfs for StageVfs {
    fn open(&self, path: &Path) -> std::io::Result<u64> {
        self.inner.open(path)
    }
    fn read(&self, path: &Path) -> std::io::Result<Vec<u8>> {
        self.inner.read(path)
    }
    fn read_range(&self, path: &Path, offset: u64, length: usize) -> std::io::Result<Vec<u8>> {
        self.inner.read_range(path, offset, length)
    }
    fn write(&self, path: &Path, bytes: &[u8]) -> std::io::Result<()> {
        self.inner.write(path, bytes)
    }
    fn open_append(&self, path: &Path) -> std::io::Result<Box<dyn VfsFile>> {
        self.inner.open_append(path)
    }
    fn rename(&self, from: &Path, to: &Path) -> std::io::Result<()> {
        if self.fail_rename {
            Err(std::io::Error::other("rename stage"))
        } else {
            self.inner.rename(from, to)
        }
    }
    fn sync(&self, path: &Path, kind: SyncKind) -> std::io::Result<()> {
        let call = self.sync_calls.fetch_add(1, Ordering::Relaxed) + 1;
        if call == self.fail_sync_call {
            Err(std::io::Error::other(format!("sync stage {call}")))
        } else {
            self.inner.sync(path, kind)
        }
    }
    fn list(&self, directory: &Path) -> std::io::Result<Vec<std::path::PathBuf>> {
        self.inner.list(directory)
    }
    fn delete(&self, path: &Path) -> std::io::Result<()> {
        self.inner.delete(path)
    }
}

#[test]
fn vfs_open_append_preserves_existing_bytes_and_syncs_the_open_handle() {
    let directory = tempdir().expect("tempdir");
    let path = directory.path().join("wal.ze");
    std::fs::write(&path, b"prefix").expect("prefix");
    let mut handle = StdVfs.open_append(&path).expect("append handle");
    handle.append(b"-one").expect("first append");
    handle.append(b"-two").expect("second append");
    handle.sync(SyncKind::Barrier).expect("barrier");
    drop(handle);
    assert_eq!(std::fs::read(&path).expect("read"), b"prefix-one-two");
}

#[test]
fn segment_writer_names_sync_rename_and_directory_sync_failures() {
    let columns = empty_columns();
    let alive = AliveSet::new(0);
    for (fail_sync_call, fail_rename, expected) in [
        (1, false, "sync stage 1"),
        (0, true, "rename stage"),
        (2, false, "sync stage 2"),
    ] {
        let directory = tempdir().expect("tempdir");
        let vfs = StageVfs {
            inner: StdVfs,
            fail_sync_call,
            fail_rename,
            sync_calls: AtomicUsize::new(0),
        };
        let error = write_segment(
            &vfs,
            directory.path(),
            SegmentBuild {
                id: SegmentId::new(fail_sync_call as u64, [fail_rename as u8; 10]),
                scheme: 4,
                dims: 3,
                codes: &[],
                factors: SegmentFactors::Bit4(&[]),
                rescore: &[],
                columns: &columns,
                alive: &alive,
            },
            ordered_policy(),
        )
        .expect_err("commit stage");
        assert!(error.to_string().contains(expected), "{error}");
    }
}
