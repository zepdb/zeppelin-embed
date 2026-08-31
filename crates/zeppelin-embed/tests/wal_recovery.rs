#![allow(
    clippy::expect_used,
    clippy::indexing_slicing,
    clippy::panic,
    clippy::unwrap_used
)]

use std::collections::BTreeMap;
use std::io::IoSlice;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::Barrier;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::thread;
use std::time::{Duration, Instant};

#[cfg(unix)]
use std::fs::OpenOptions;
#[cfg(unix)]
use std::io::Write;
#[cfg(unix)]
use std::os::fd::AsRawFd;
#[cfg(unix)]
use std::os::unix::process::ExitStatusExt;
#[cfg(unix)]
use std::process::{Command, Stdio};

use rand::{Rng, SeedableRng};
use rand_chacha::ChaCha8Rng;
use tempfile::tempdir;
use zeppelin_embed::epoch::{
    ComputeUnits, EmbeddingEpoch, EmbeddingRuntime, EmbeddingTower, Normalization, StoreEpoch,
};
use zeppelin_embed::fts::tokenizer::TokenizerConfig;
use zeppelin_embed::graph::search::GraphSearchProfile;
use zeppelin_embed::ingest::{
    DeleteBatch, DocId, DocumentVersion, IngestBatch, IngestDocument, IngestError, Revision,
    SearchRequest,
};
use zeppelin_embed::lifecycle::durability::{CommitTier, DurabilityMode, DurabilityPolicy};
use zeppelin_embed::lifecycle::lock::{STORE_LOCK_FILE, StoreLock};
use zeppelin_embed::lifecycle::{
    CancelToken, GraphSearchOptions, OpenOptions as StoreOpenOptions, QueryControl, SearchOptions,
    SearchTier, Store, StoreError, StoreTestDependencies, SystemMonotonicClock,
};
use zeppelin_embed::manifest::io::{DurableLog, MANIFEST_FILE, load_manifest};
use zeppelin_embed::manifest::{Manifest, ManifestError, encode_manifest};
use zeppelin_embed::meta::{Predicate, PredicateValue, Schema, TIMESTAMP_COLUMN};
use zeppelin_embed::tier::{MaintenanceBudget, MaintenanceStatus, TierThresholds};
use zeppelin_embed::vfs::StdVfs;
use zeppelin_embed::vfs::{CountingVfs, SyncKind, Vfs, VfsFile};
#[cfg(unix)]
use zeppelin_embed::wal::header::WAL_HEADER_LEN;
#[cfg(unix)]
use zeppelin_embed::wal::record::MIN_RECORD_LEN;
use zeppelin_embed::wal::{
    GROUP_SIZE_HISTOGRAM_BUCKETS, LogSeq, RECENT_GROUP_LIMIT, WalReader, WalRetireError, WalWriter,
};

#[path = "../src/vfs/fault.rs"]
mod fault_support;

use fault_support::{BlockingVfs, FaultImage, FaultVfs};

#[path = "../src/vfs/crash.rs"]
#[allow(dead_code)]
mod crash_support;

use crash_support::{CrashOperation, CrashStateClass, CrashStateKind, CrashVfs, MemoryVfs};

const WAL_PATH: &str = "/wal-recovery/wal.ze";
const SEALED_RECOVERY_DIMS: usize = 128;
const SEALED_RECOVERY_ROWS: usize = 8;

fn policy(tier: CommitTier) -> DurabilityPolicy {
    DurabilityPolicy::new(DurabilityMode::Durable, tier).expect("supported policy")
}

struct PayloadPointerVfs<V> {
    inner: V,
    appended_payload: Arc<AtomicUsize>,
    appended_buffers: Arc<AtomicUsize>,
}

struct PayloadPointerVfsFile {
    inner: Box<dyn VfsFile>,
    appended_payload: Arc<AtomicUsize>,
    appended_buffers: Arc<AtomicUsize>,
}

struct BlockingAppendVfs<V> {
    inner: V,
    target_call: usize,
    append_calls: Arc<AtomicUsize>,
    blocked: Arc<Barrier>,
    release: Arc<Barrier>,
}

struct BlockingAppendVfsFile {
    inner: Box<dyn VfsFile>,
    target_call: usize,
    append_calls: Arc<AtomicUsize>,
    blocked: Arc<Barrier>,
    release: Arc<Barrier>,
}

#[derive(Clone)]
struct FailNextManifestRenameVfs {
    inner: Arc<dyn Vfs>,
    armed: Arc<AtomicBool>,
}

impl FailNextManifestRenameVfs {
    fn new() -> Self {
        Self {
            inner: Arc::new(StdVfs),
            armed: Arc::new(AtomicBool::new(false)),
        }
    }

    fn arm(&self) {
        self.armed.store(true, Ordering::Release);
    }

    fn is_armed(&self) -> bool {
        self.armed.load(Ordering::Acquire)
    }
}

impl Vfs for FailNextManifestRenameVfs {
    fn ensure_directory(&self, path: &Path, create: bool) -> std::io::Result<bool> {
        self.inner.ensure_directory(path, create)
    }

    fn open(&self, path: &Path) -> std::io::Result<u64> {
        self.inner.open(path)
    }

    fn open_for_map(&self, path: &Path) -> std::io::Result<std::fs::File> {
        self.inner.open_for_map(path)
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
        if to.file_name().is_some_and(|name| name == MANIFEST_FILE)
            && self.armed.swap(false, Ordering::AcqRel)
        {
            return Err(std::io::Error::from_raw_os_error(libc::EIO));
        }
        self.inner.rename(from, to)
    }

    fn sync(&self, path: &Path, kind: SyncKind) -> std::io::Result<()> {
        self.inner.sync(path, kind)
    }

    fn list(&self, directory: &Path) -> std::io::Result<Vec<PathBuf>> {
        self.inner.list(directory)
    }

    fn delete(&self, path: &Path) -> std::io::Result<()> {
        self.inner.delete(path)
    }
}

impl<V: Clone> Clone for BlockingAppendVfs<V> {
    fn clone(&self) -> Self {
        Self {
            inner: self.inner.clone(),
            target_call: self.target_call,
            append_calls: Arc::clone(&self.append_calls),
            blocked: Arc::clone(&self.blocked),
            release: Arc::clone(&self.release),
        }
    }
}

impl<V> BlockingAppendVfs<V> {
    fn new(inner: V, target_call: usize) -> Self {
        Self {
            inner,
            target_call,
            append_calls: Arc::new(AtomicUsize::new(0)),
            blocked: Arc::new(Barrier::new(2)),
            release: Arc::new(Barrier::new(2)),
        }
    }

    fn wait_until_blocked(&self) {
        self.blocked.wait();
    }

    fn release(&self) {
        self.release.wait();
    }
}

impl BlockingAppendVfsFile {
    fn block_target_append(&self) {
        let call = self.append_calls.fetch_add(1, Ordering::AcqRel) + 1;
        if call == self.target_call {
            self.blocked.wait();
            self.release.wait();
        }
    }
}

impl VfsFile for BlockingAppendVfsFile {
    fn append(&mut self, bytes: &[u8]) -> std::io::Result<()> {
        self.block_target_append();
        self.inner.append(bytes)
    }

    fn append_vectored(&mut self, buffers: &mut [IoSlice<'_>]) -> std::io::Result<()> {
        self.block_target_append();
        self.inner.append_vectored(buffers)
    }

    fn sync(&self, kind: SyncKind) -> std::io::Result<()> {
        self.inner.sync(kind)
    }
}

impl<V: Vfs> Vfs for BlockingAppendVfs<V> {
    fn ensure_directory(&self, path: &Path, create: bool) -> std::io::Result<bool> {
        self.inner.ensure_directory(path, create)
    }

    fn open(&self, path: &Path) -> std::io::Result<u64> {
        self.inner.open(path)
    }

    fn open_for_map(&self, path: &Path) -> std::io::Result<std::fs::File> {
        self.inner.open_for_map(path)
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
        Ok(Box::new(BlockingAppendVfsFile {
            inner: self.inner.open_append(path)?,
            target_call: self.target_call,
            append_calls: Arc::clone(&self.append_calls),
            blocked: Arc::clone(&self.blocked),
            release: Arc::clone(&self.release),
        }))
    }

    fn rename(&self, from: &Path, to: &Path) -> std::io::Result<()> {
        self.inner.rename(from, to)
    }

    fn sync(&self, path: &Path, kind: SyncKind) -> std::io::Result<()> {
        self.inner.sync(path, kind)
    }

    fn list(&self, directory: &Path) -> std::io::Result<Vec<PathBuf>> {
        self.inner.list(directory)
    }

    fn delete(&self, path: &Path) -> std::io::Result<()> {
        self.inner.delete(path)
    }
}

impl<V> PayloadPointerVfs<V> {
    fn new(inner: V) -> Self {
        Self {
            inner,
            appended_payload: Arc::new(AtomicUsize::new(0)),
            appended_buffers: Arc::new(AtomicUsize::new(0)),
        }
    }

    fn appended_payload(&self) -> usize {
        self.appended_payload.load(Ordering::Acquire)
    }

    fn appended_buffers(&self) -> usize {
        self.appended_buffers.load(Ordering::Acquire)
    }
}

impl VfsFile for PayloadPointerVfsFile {
    fn append(&mut self, bytes: &[u8]) -> std::io::Result<()> {
        self.appended_buffers.store(1, Ordering::Release);
        self.appended_payload.store(
            bytes.as_ptr() as usize
                + zeppelin_embed::wal::header::WAL_HEADER_LEN
                + zeppelin_embed::wal::record::RECORD_HEADER_LEN,
            Ordering::Release,
        );
        self.inner.append(bytes)
    }

    fn append_vectored(&mut self, buffers: &mut [IoSlice<'_>]) -> std::io::Result<()> {
        self.appended_buffers
            .store(buffers.len(), Ordering::Release);
        if let Some(record) = buffers.last() {
            self.appended_payload.store(
                record.as_ptr() as usize + zeppelin_embed::wal::record::RECORD_HEADER_LEN,
                Ordering::Release,
            );
        }
        self.inner.append_vectored(buffers)
    }

    fn sync(&self, kind: SyncKind) -> std::io::Result<()> {
        self.inner.sync(kind)
    }
}

impl<V: Vfs> Vfs for PayloadPointerVfs<V> {
    fn ensure_directory(&self, path: &Path, create: bool) -> std::io::Result<bool> {
        self.inner.ensure_directory(path, create)
    }

    fn open(&self, path: &Path) -> std::io::Result<u64> {
        self.inner.open(path)
    }

    fn open_for_map(&self, path: &Path) -> std::io::Result<std::fs::File> {
        self.inner.open_for_map(path)
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
        Ok(Box::new(PayloadPointerVfsFile {
            inner: self.inner.open_append(path)?,
            appended_payload: Arc::clone(&self.appended_payload),
            appended_buffers: Arc::clone(&self.appended_buffers),
        }))
    }

    fn rename(&self, from: &Path, to: &Path) -> std::io::Result<()> {
        self.inner.rename(from, to)
    }

    fn sync(&self, path: &Path, kind: SyncKind) -> std::io::Result<()> {
        self.inner.sync(path, kind)
    }

    fn list(&self, directory: &Path) -> std::io::Result<Vec<PathBuf>> {
        self.inner.list(directory)
    }

    fn delete(&self, path: &Path) -> std::io::Result<()> {
        self.inner.delete(path)
    }
}

fn recovered_sequences(image: &FaultImage) -> Vec<u64> {
    WalReader::open(image, Path::new(WAL_PATH))
        .expect("recovery needs no manual steps")
        .records()
        .iter()
        .map(|record| record.seq.get())
        .collect()
}

#[test]
fn commit_reuses_encoded_payload_for_visibility() {
    let vfs = PayloadPointerVfs::new(MemoryVfs::new());
    let writer = WalWriter::create(
        &vfs,
        Path::new(WAL_PATH),
        LogSeq::new(1),
        policy(CommitTier::None),
    )
    .expect("writer");

    writer
        .commit_durable(7, &[0x5a; 1_024])
        .expect("commit payload");
    let visible = writer
        .visible_records(LogSeq::new(1), 1)
        .expect("visible records");

    assert_eq!(
        visible[0].payload().expect("checked payload").as_ptr() as usize,
        vfs.appended_payload(),
        "visibility must borrow the payload bytes already owned by the encoded WAL record"
    );
}

#[test]
fn commit_many_uses_one_append_and_one_sync_for_one_group() {
    let counting = CountingVfs::new(MemoryVfs::new());
    let writer = WalWriter::create_with_max_group_bytes(
        &counting,
        Path::new(WAL_PATH),
        LogSeq::new(1),
        policy(CommitTier::Durable),
        1_024,
    )
    .expect("writer");
    let payloads = [[0x11; 10], [0x22; 10], [0x33; 10], [0x44; 10]];
    let records = payloads
        .iter()
        .enumerate()
        .map(|(index, payload)| (index as u16, payload.as_slice()))
        .collect::<Vec<_>>();

    let range = writer.commit_many(&records).expect("batch commit");
    let stats = writer.stats().expect("stats");

    assert_eq!(
        (
            range,
            counting.append_calls(),
            counting.handle_barrier_sync_calls(),
            counting.handle_full_sync_calls(),
            counting.bytes_appended(),
            stats.recent_groups,
        ),
        (
            LogSeq::new(1)..LogSeq::new(5),
            1,
            0,
            1,
            168,
            vec![zeppelin_embed::wal::CompletedGroup {
                records: 4,
                encoded_bytes: 168,
            }],
        ),
        "four records inside the byte bound must share exactly one append and full sync"
    );
}

#[test]
fn commit_many_uses_one_shared_encoded_allocation() {
    let vfs = PayloadPointerVfs::new(MemoryVfs::new());
    let writer = WalWriter::create(
        &vfs,
        Path::new(WAL_PATH),
        LogSeq::new(1),
        policy(CommitTier::None),
    )
    .expect("writer");
    writer.commit(0, b"prime").expect("flush WAL header");
    writer
        .retire_visible_through(LogSeq::new(1))
        .expect("retire prime record");
    let allocations_before = writer
        .stats()
        .expect("stats before batch")
        .encoded_buffer_allocations;
    let payloads = [[0x51; 10], [0x52; 10], [0x53; 10], [0x54; 10]];
    let records = payloads
        .iter()
        .map(|payload| (9, payload.as_slice()))
        .collect::<Vec<_>>();

    writer.commit_many(&records).expect("batch commit");
    let visible = writer
        .visible_records(LogSeq::new(2), records.len())
        .expect("visible batch");
    let allocations_after = writer
        .stats()
        .expect("stats after batch")
        .encoded_buffer_allocations;
    let first_payload = visible
        .first()
        .expect("first visible batch record")
        .payload()
        .expect("checked payload")
        .as_ptr() as usize;

    assert_eq!(
        (
            allocations_after.saturating_sub(allocations_before),
            vfs.appended_buffers(),
            first_payload == vfs.appended_payload(),
        ),
        (1, 1, true),
        "one batch must own one shared encoded allocation and append it as one iovec"
    );
}

#[test]
fn commit_many_splits_oversized_batch_into_exact_groups() {
    let counting = CountingVfs::new(MemoryVfs::new());
    let writer = WalWriter::create_with_max_group_bytes(
        &counting,
        Path::new(WAL_PATH),
        LogSeq::new(1),
        policy(CommitTier::Durable),
        128,
    )
    .expect("writer");
    writer.commit(0, &[0; 10]).expect("flush WAL header");
    let groups_before = writer.stats().expect("prime stats").recent_groups.len();
    counting.reset();
    let payloads = [[0x61; 10]; 12];
    let records = payloads
        .iter()
        .map(|payload| (11, payload.as_slice()))
        .collect::<Vec<_>>();

    let result = writer.commit_many(&records);
    let actual = match result {
        Ok(range) => {
            let stats = writer.stats().expect("batch stats");
            let groups = stats
                .recent_groups
                .iter()
                .skip(groups_before)
                .map(|group| (group.records, group.encoded_bytes))
                .collect::<Vec<_>>();
            format!(
                "ok range={}..{} appends={} full_syncs={} bytes={} groups={groups:?}",
                range.start.get(),
                range.end.get(),
                counting.append_calls(),
                counting.handle_full_sync_calls(),
                counting.bytes_appended(),
            )
        }
        Err(error) => format!("error={error}"),
    };

    assert_eq!(
        actual,
        "ok range=2..14 appends=3 full_syncs=3 bytes=384 groups=[(4, 128), (4, 128), (4, 128)]",
        "384 encoded batch bytes must split into exactly ceil(384/128) groups"
    );
}

#[test]
fn commit_many_mixed_sizes_still_uses_ceil_byte_groups() {
    let counting = CountingVfs::new(MemoryVfs::new());
    let writer = WalWriter::create_with_max_group_bytes(
        &counting,
        Path::new(WAL_PATH),
        LogSeq::new(1),
        policy(CommitTier::Durable),
        128,
    )
    .expect("writer");
    writer.commit(0, &[0; 10]).expect("flush WAL header");
    let groups_before = writer.stats().expect("prime stats").recent_groups.len();
    counting.reset();
    let payloads = [[0x62; 48]; 3];
    let records = payloads
        .iter()
        .map(|payload| (12, payload.as_slice()))
        .collect::<Vec<_>>();

    let range = writer.commit_many(&records).expect("batch commit");
    let stats = writer.stats().expect("batch stats");
    let groups = stats
        .recent_groups
        .iter()
        .skip(groups_before)
        .map(|group| (group.records, group.encoded_bytes))
        .collect::<Vec<_>>();
    let actual = format!(
        "range={}..{} appends={} full_syncs={} bytes={} groups={groups:?}",
        range.start.get(),
        range.end.get(),
        counting.append_calls(),
        counting.handle_full_sync_calls(),
        counting.bytes_appended(),
    );

    assert_eq!(
        actual, "range=2..5 appends=2 full_syncs=2 bytes=210 groups=[(1, 128), (2, 82)]",
        "mixed record sizes must still produce exactly ceil(batch_bytes/max_group_bytes) groups"
    );
}

#[test]
fn commit_many_returns_gap_free_sequences_visible_in_input_order() {
    let writer = WalWriter::create(
        &MemoryVfs::new(),
        Path::new(WAL_PATH),
        LogSeq::new(10),
        policy(CommitTier::None),
    )
    .expect("writer");
    let payloads = [b"alpha".as_slice(), b"beta", b"gamma", b"delta", b"epsilon"];
    let records = payloads
        .iter()
        .enumerate()
        .map(|(index, payload)| (20 + index as u16, *payload))
        .collect::<Vec<_>>();

    let range = writer.commit_many(&records).expect("batch commit");
    let visible = writer
        .visible_records(LogSeq::new(10), records.len())
        .expect("visible batch");
    let actual = visible
        .iter()
        .map(|record| {
            (
                record.seq.get(),
                record.op,
                record.payload().expect("checked payload").to_vec(),
            )
        })
        .collect::<Vec<_>>();
    let expected = records
        .iter()
        .enumerate()
        .map(|(index, (op, payload))| (10 + index as u64, *op, payload.to_vec()))
        .collect::<Vec<_>>();

    assert_eq!(
        (range, actual),
        (LogSeq::new(10)..LogSeq::new(15), expected),
        "the returned range and visible queue must contain the same gap-free input-ordered sequences"
    );
}

#[test]
fn one_record_commit_many_matches_commit_exactly() {
    let single_vfs = CountingVfs::new(MemoryVfs::new());
    let single_writer = WalWriter::create(
        &single_vfs,
        Path::new("/wal-recovery/single.ze"),
        LogSeq::new(1),
        policy(CommitTier::Durable),
    )
    .expect("single writer");
    let single_seq = single_writer
        .commit(17, b"same-record")
        .expect("single commit");
    let single_stats = single_writer.stats().expect("single stats");
    let single_shape = (
        single_vfs.append_calls(),
        single_vfs.handle_barrier_sync_calls(),
        single_vfs.handle_full_sync_calls(),
        single_vfs.bytes_appended(),
        single_stats.recent_groups,
        single_stats.encoded_buffer_allocations,
    );

    let batch_vfs = CountingVfs::new(MemoryVfs::new());
    let batch_writer = WalWriter::create(
        &batch_vfs,
        Path::new("/wal-recovery/batch.ze"),
        LogSeq::new(1),
        policy(CommitTier::Durable),
    )
    .expect("batch writer");
    let empty_range = batch_writer.commit_many(&[]).expect("empty batch");
    let empty_durable_range = batch_writer
        .commit_many_durable(&[])
        .expect("empty durable batch");
    let empty_stats = batch_writer.stats().expect("empty batch stats");
    let empty_shape = (
        batch_vfs.append_calls(),
        batch_vfs.handle_barrier_sync_calls(),
        batch_vfs.handle_full_sync_calls(),
        batch_vfs.bytes_appended(),
        empty_stats.recent_groups,
        empty_stats.encoded_buffer_allocations,
    );
    let records = [(17, b"same-record".as_slice())];
    let batch_range = batch_writer
        .commit_many(&records)
        .expect("one-record batch");
    let batch_stats = batch_writer.stats().expect("batch stats");
    let batch_shape = (
        batch_vfs.append_calls(),
        batch_vfs.handle_barrier_sync_calls(),
        batch_vfs.handle_full_sync_calls(),
        batch_vfs.bytes_appended(),
        batch_stats.recent_groups,
        batch_stats.encoded_buffer_allocations,
    );

    assert_eq!(
        (
            empty_range,
            empty_durable_range,
            empty_shape,
            batch_range,
            batch_shape,
        ),
        (
            LogSeq::new(1)..LogSeq::new(1),
            LogSeq::new(1)..LogSeq::new(1),
            (0, 0, 0, 0, Vec::new(), 0),
            single_seq..LogSeq::new(single_seq.get().saturating_add(1)),
            single_shape,
        ),
        "a one-record batch must have the same result, counters, and group composition as commit"
    );
}

#[test]
fn commit_many_staging_failure_consumes_nothing() {
    let writer = WalWriter::create(
        &MemoryVfs::new(),
        Path::new(WAL_PATH),
        LogSeq::new(u64::MAX - 1),
        policy(CommitTier::None),
    )
    .expect("writer");
    let records = [(31, b"first".as_slice()), (32, b"second".as_slice())];

    let batch = match writer.commit_many(&records) {
        Ok(range) => format!("ok={}..{}", range.start.get(), range.end.get()),
        Err(error) => format!("error={error}"),
    };
    let visible = writer
        .visible_records(LogSeq::new(u64::MAX - 1), records.len())
        .expect("visibility after failed staging")
        .iter()
        .map(|record| record.seq.get())
        .collect::<Vec<_>>();
    let next = match writer.commit(33, b"after-failure") {
        Ok(seq) => format!("ok={}", seq.get()),
        Err(error) => format!("error={error}"),
    };
    let actual = format!("batch={batch} visible={visible:?} next={next}");

    assert_eq!(
        actual,
        format!(
            "batch=error=WAL sequence space exhausted visible=[] next=ok={}",
            u64::MAX - 1
        ),
        "a batch that cannot assign every record must publish nothing and leave its first sequence reusable"
    );
}

#[test]
fn commit_many_durable_stages_all_then_waits_for_the_last_flush() {
    let blocking = BlockingVfs::new(FaultVfs::new());
    blocking.block_next_syncs(1).expect("arm batch barrier");
    let counting = CountingVfs::new(blocking.clone());
    let writer = Arc::new(
        WalWriter::create(
            &counting,
            Path::new(WAL_PATH),
            LogSeq::new(1),
            policy(CommitTier::Ordered),
        )
        .expect("writer"),
    );
    let returned = Arc::new(AtomicBool::new(false));
    let committing_writer = Arc::clone(&writer);
    let committing_returned = Arc::clone(&returned);
    let commit = thread::spawn(move || {
        let payloads = [[0x71; 10], [0x72; 10], [0x73; 10], [0x74; 10]];
        let records = payloads
            .iter()
            .map(|payload| (41, payload.as_slice()))
            .collect::<Vec<_>>();
        let result = committing_writer.commit_many_durable(&records);
        committing_returned.store(true, Ordering::Release);
        result
    });

    blocking
        .wait_until_blocked(1)
        .expect("batch barrier blocked");
    let visible_while_blocked = writer
        .visible_records(LogSeq::new(1), 4)
        .expect("visible batch while barrier is blocked")
        .len();
    let returned_while_blocked = returned.load(Ordering::Acquire);
    blocking.release_syncs(1).expect("release batch barrier");
    let range = commit
        .join()
        .expect("batch commit thread")
        .expect("durable batch");

    assert_eq!(
        (
            visible_while_blocked,
            returned_while_blocked,
            range,
            counting.append_calls(),
            counting.handle_barrier_sync_calls(),
            counting.handle_full_sync_calls(),
        ),
        (4, false, LogSeq::new(1)..LogSeq::new(5), 1, 1, 0,),
        "the durable batch must stage every record atomically and return only after its covering flush"
    );
}

#[test]
fn commit_many_durable_waits_for_the_last_group_not_the_first() {
    let fault = FaultVfs::new();
    let last_group_append = BlockingAppendVfs::new(fault.clone(), 3);
    let blocking = BlockingVfs::new(last_group_append.clone());
    blocking.block_next_syncs(1).expect("arm first sync only");
    let writer = Arc::new(
        WalWriter::create_with_max_group_bytes(
            &blocking,
            Path::new(WAL_PATH),
            LogSeq::new(1),
            policy(CommitTier::Durable),
            128,
        )
        .expect("writer"),
    );

    let leading_writer = Arc::clone(&writer);
    let leading_commit = thread::spawn(move || leading_writer.commit_durable(40, &[0x70; 10]));
    blocking.wait_until_blocked(1).expect("first sync blocked");

    let returned = Arc::new(AtomicBool::new(false));
    let committing_writer = Arc::clone(&writer);
    let committing_returned = Arc::clone(&returned);
    let batch_commit = thread::spawn(move || {
        let payloads = [[0x71; 10], [0x72; 10], [0x73; 10], [0x74; 10], [0x75; 10]];
        let records = payloads
            .iter()
            .map(|payload| (41, payload.as_slice()))
            .collect::<Vec<_>>();
        let result = committing_writer.commit_many_durable(&records);
        committing_returned.store(true, Ordering::Release);
        result
    });

    while writer
        .visible_records(LogSeq::new(1), 6)
        .expect("visible records")
        .len()
        < 6
    {
        thread::yield_now();
    }
    let returned_while_first_sync_blocked = returned.load(Ordering::Acquire);

    blocking.release_syncs(1).expect("release first sync");
    last_group_append.wait_until_blocked();
    for _ in 0..10_000 {
        if returned.load(Ordering::Acquire) {
            break;
        }
        thread::yield_now();
    }
    let returned_while_last_group_unsynced = returned.load(Ordering::Acquire);
    last_group_append.release();

    leading_commit
        .join()
        .expect("leading commit thread")
        .expect("leading durable commit");
    let range = batch_commit
        .join()
        .expect("batch commit thread")
        .expect("durable batch");
    let stats = writer.stats().expect("stats");
    let group_sizes = stats
        .recent_groups
        .iter()
        .map(|group| group.records)
        .collect::<Vec<_>>();
    let recovered = recovered_sequences(&fault.power_cut().expect("power cut"));

    assert_eq!(
        (
            returned_while_first_sync_blocked,
            returned_while_last_group_unsynced,
            range,
            stats.durable_end,
            group_sizes,
            recovered,
        ),
        (
            false,
            false,
            LogSeq::new(2)..LogSeq::new(7),
            Some(LogSeq::new(6)),
            vec![1, 4, 1],
            vec![1, 2, 3, 4, 5, 6],
        ),
        "the durable batch must wait for its second group before returning"
    );
}

#[test]
fn retirement_refuses_a_visible_record_until_its_flush_returns() {
    for first_sequence in [0, 1] {
        let blocking = BlockingVfs::new(FaultVfs::new());
        blocking.block_next_syncs(1).expect("arm first barrier");
        let writer = Arc::new(
            WalWriter::create(
                &blocking,
                Path::new(WAL_PATH),
                LogSeq::new(first_sequence),
                policy(CommitTier::Ordered),
            )
            .expect("writer"),
        );

        let committing_writer = Arc::clone(&writer);
        let commit = thread::spawn(move || committing_writer.commit_durable(7, &[0x41; 64]));
        blocking.wait_until_blocked(1).expect("barrier blocked");

        let requested = LogSeq::new(first_sequence);
        let error = writer
            .retire_visible_through(requested)
            .expect_err("visible but non-durable record must not retire");
        assert!(
            matches!(
                error,
                WalRetireError::BeyondDurable {
                    requested: actual,
                    durable_end: None
                } if actual == requested
            ),
            "retirement past the durable end must return the typed boundary error: {error:?}"
        );
        assert_eq!(writer.stats().expect("stats").retained_records, 1);

        blocking.release_syncs(1).expect("release barrier");
        commit
            .join()
            .expect("commit thread")
            .expect("durable commit");
        let past_durable = LogSeq::new(first_sequence + 1);
        assert!(
            matches!(
                writer.retire_visible_through(past_durable),
                Err(WalRetireError::BeyondDurable {
                    requested: actual,
                    durable_end: Some(actual_durable),
                }) if actual == past_durable && actual_durable == requested
            ),
            "retirement past a non-empty durable prefix must fail rather than clamp"
        );
        let retired = writer
            .retire_visible_through(requested)
            .expect("durable record retires");
        assert_eq!(
            (
                retired.records_released,
                retired.encoded_bytes_released,
                retired.retained_records,
                retired.retained_bytes,
                retired.durable_end,
            ),
            (1, 86, 0, 0, Some(requested)),
            "retirement must release the one shared encoded record allocation"
        );
    }
}

#[test]
fn retirement_keeps_writer_retained_bytes_bounded() {
    const BATCH: u64 = 8;
    const COMMITS: u64 = 128;
    const ENCODED_RECORD_BYTES: usize = 64 + zeppelin_embed::wal::record::MIN_RECORD_LEN;

    let writer = WalWriter::create(
        &MemoryVfs::new(),
        Path::new(WAL_PATH),
        LogSeq::new(1),
        policy(CommitTier::None),
    )
    .expect("writer");
    let mut peak_retained_bytes = 0_usize;

    for sequence in 1..=COMMITS {
        writer
            .commit_durable(9, &[0x42; 64])
            .expect("bounded commit");
        let retained = writer.stats().expect("stats").retained_bytes;
        peak_retained_bytes = peak_retained_bytes.max(retained);
        if sequence % BATCH == 0 {
            writer
                .retire_visible_through(LogSeq::new(sequence))
                .expect("batch retirement");
        }
    }

    let stats = writer.stats().expect("final stats");
    assert_eq!((stats.retained_records, stats.retained_bytes), (0, 0));
    assert_eq!(
        peak_retained_bytes,
        BATCH as usize * ENCODED_RECORD_BYTES,
        "retained encoded bytes must be bounded by the caller's retirement cadence"
    );
}

#[test]
fn wal_bytes_grow_monotonically_without_retirement() {
    const RECORDS: usize = 128;
    const PAYLOAD_BYTES: usize = 40;
    const GROWTH_PER_RECORD: usize = PAYLOAD_BYTES + MIN_RECORD_LEN;

    let writer = WalWriter::create(
        &MemoryVfs::new(),
        Path::new(WAL_PATH),
        LogSeq::new(1),
        policy(CommitTier::None),
    )
    .expect("writer");
    let payload = [0x51; PAYLOAD_BYTES];
    let mut previous = 0_usize;
    for record in 1..=RECORDS {
        writer.commit_durable(12, &payload).expect("append");
        let measured = writer.stats().expect("WAL stats").retained_bytes;
        assert_eq!(
            measured,
            record * GROWTH_PER_RECORD,
            "retained WAL bytes changed by something other than one encoded record"
        );
        assert!(measured > previous, "retained WAL bytes did not grow");
        previous = measured;
    }
    eprintln!(
        "BL-081 current_retained_growth_per_record={GROWTH_PER_RECORD} payload_bytes={PAYLOAD_BYTES} records={RECORDS} final_wal_bytes={previous}"
    );
}

#[test]
fn statistics_are_fixed_size_with_exact_recent_groups() {
    let writer = WalWriter::create(
        &MemoryVfs::new(),
        Path::new(WAL_PATH),
        LogSeq::new(1),
        policy(CommitTier::None),
    )
    .expect("writer");
    let commits = RECENT_GROUP_LIMIT + 9;
    for _ in 0..commits {
        writer.commit_durable(3, &[0x43; 10]).expect("commit");
    }

    let stats = writer.stats().expect("stats");
    assert_eq!(stats.completed_groups, commits as u64);
    assert_eq!(stats.completed_records, commits as u64);
    assert_eq!(stats.recent_groups.len(), RECENT_GROUP_LIMIT);
    assert!(
        stats
            .recent_groups
            .iter()
            .all(|group| group.records == 1 && group.encoded_bytes == 32),
        "the bounded ring must retain exact count/byte pairs for its last N groups"
    );
    assert_eq!(stats.group_size_histogram.buckets[0], commits as u64);
    assert_eq!(
        stats.group_size_histogram.buckets.iter().sum::<u64>(),
        commits as u64
    );
    assert_eq!(
        stats.group_size_histogram.buckets.len(),
        GROUP_SIZE_HISTOGRAM_BUCKETS
    );
}

#[test]
fn visible_records_are_sequence_and_count_bounded() {
    let writer = WalWriter::create(
        &MemoryVfs::new(),
        Path::new(WAL_PATH),
        LogSeq::new(1),
        policy(CommitTier::None),
    )
    .expect("writer");
    for value in 1_u8..=6 {
        writer.commit_durable(2, &[value]).expect("commit");
    }

    let visible = writer
        .visible_records(LogSeq::new(3), 2)
        .expect("bounded visibility");
    assert_eq!(
        visible
            .iter()
            .map(|record| record.seq.get())
            .collect::<Vec<_>>(),
        vec![3, 4]
    );
    assert_eq!(
        visible
            .iter()
            .map(|record| record.payload().expect("checked payload")[0])
            .collect::<Vec<_>>(),
        vec![3, 4]
    );
}

#[derive(Clone, Copy, Debug)]
enum Workload {
    Ingest,
    Seal,
    Publish,
}

impl Workload {
    const fn records(self) -> usize {
        match self {
            Self::Ingest => 2,
            Self::Seal => 3,
            Self::Publish => 4,
        }
    }
}

fn operation_boundary(kind: &CrashStateKind) -> usize {
    match kind {
        CrashStateKind::Prefix {
            completed_operations,
        } => *completed_operations,
        CrashStateKind::TornWrite {
            operation_index, ..
        }
        | CrashStateKind::ExtendedWithGarbage {
            operation_index, ..
        }
        | CrashStateKind::ExtendedWithZeros {
            operation_index, ..
        }
        | CrashStateKind::InteriorDamage {
            operation_index, ..
        } => *operation_index,
        CrashStateKind::ReorderedWrites {
            through_operation, ..
        } => *through_operation,
        CrashStateKind::RenameWithOldContent { operation_index } => {
            operation_index.saturating_add(1)
        }
    }
}

fn completed_full_groups(kind: &CrashStateKind, operations: &[CrashOperation]) -> usize {
    operations
        .iter()
        .take(operation_boundary(kind))
        .filter(|operation| {
            matches!(
                operation,
                CrashOperation::Sync {
                    kind: SyncKind::Full,
                    ..
                }
            )
        })
        .count()
}

fn crash_class_counts(states: &crash_support::CrashStates) -> BTreeMap<CrashStateClass, usize> {
    let mut counts = BTreeMap::new();
    for state in states.iter() {
        *counts.entry(state.kind().class()).or_insert(0) += 1;
    }
    counts
}

#[test]
fn wal_preallocation_crash_state_class_delta_is_exact() {
    let recorder = CrashVfs::new(MemoryVfs::new()).expect("recorder");
    let writer = WalWriter::create(
        &recorder,
        Path::new(WAL_PATH),
        LogSeq::new(1),
        policy(CommitTier::Ordered),
    )
    .expect("writer");
    writer.commit_durable(1, &[0x5a; 32]).expect("commit");
    let states = recorder.crash_states().expect("states");
    let without = crash_class_counts(&states);
    let without_vector = [
        CrashStateClass::Prefix,
        CrashStateClass::TornWrite,
        CrashStateClass::ExtendedWithGarbage,
        CrashStateClass::ExtendedWithZeros,
        CrashStateClass::InteriorDamage,
        CrashStateClass::ReorderedWrites,
        CrashStateClass::RenameWithOldContent,
    ]
    .map(|class| without.get(&class).copied().unwrap_or(0));

    // Once a finite-capacity WAL has been preallocated and zero-sized to that
    // capacity, a later append is an overwrite. Prefix/suffix and interior
    // overwrite damage remain; allocation-extension garbage is impossible and
    // zero-tail extension collapses into the existing prefix-overwrite state.
    let preallocated_states = states
        .iter()
        .filter(|state| {
            !matches!(
                state.kind().class(),
                CrashStateClass::ExtendedWithGarbage | CrashStateClass::ExtendedWithZeros
            )
        })
        .collect::<Vec<_>>();
    let mut with = BTreeMap::new();
    for state in &preallocated_states {
        *with.entry(state.kind().class()).or_insert(0) += 1;
    }
    let with_vector = [
        CrashStateClass::Prefix,
        CrashStateClass::TornWrite,
        CrashStateClass::ExtendedWithGarbage,
        CrashStateClass::ExtendedWithZeros,
        CrashStateClass::InteriorDamage,
        CrashStateClass::ReorderedWrites,
        CrashStateClass::RenameWithOldContent,
    ]
    .map(|class| with.get(&class).copied().unwrap_or(0));
    let with_total = preallocated_states.len();
    eprintln!(
        "wal_preallocation without_total={} without={without_vector:?} with_total={with_total} with={with_vector:?}",
        states.len()
    );
    assert_eq!(
        (states.len(), without_vector, with_total, with_vector),
        (54, [4, 20, 10, 10, 10, 0, 0], 34, [4, 20, 0, 0, 10, 0, 0],),
        "finite-capacity preallocation model must expose exact class delta"
    );
}

#[test]
fn exact_flush_counts_per_tier_and_workload() {
    let mut actual = Vec::new();
    for tier in [CommitTier::None, CommitTier::Ordered, CommitTier::Durable] {
        for workload in [Workload::Ingest, Workload::Seal, Workload::Publish] {
            let counting = CountingVfs::new(MemoryVfs::new());
            let writer =
                WalWriter::create(&counting, Path::new(WAL_PATH), LogSeq::new(1), policy(tier))
                    .expect("writer");
            for index in 0..workload.records() {
                writer
                    .commit_durable(index as u16 + 1, &[index as u8; 32])
                    .expect("commit");
            }
            let row = (
                tier,
                workload.records(),
                counting.append_calls(),
                counting.handle_barrier_sync_calls(),
                counting.handle_full_sync_calls(),
            );
            eprintln!(
                "wal_flush_counts tier={tier:?} workload={workload:?} records={} appends={} barrier={} full={}",
                row.1, row.2, row.3, row.4
            );
            actual.push(row);
        }
    }
    assert_eq!(
        actual,
        vec![
            (CommitTier::None, 2, 2, 0, 0),
            (CommitTier::None, 3, 3, 0, 0),
            (CommitTier::None, 4, 4, 0, 0),
            (CommitTier::Ordered, 2, 2, 2, 0),
            (CommitTier::Ordered, 3, 3, 3, 0),
            (CommitTier::Ordered, 4, 4, 4, 0),
            (CommitTier::Durable, 2, 2, 0, 2),
            (CommitTier::Durable, 3, 3, 0, 3),
            (CommitTier::Durable, 4, 4, 0, 4),
        ],
        "every workload needs exact append and SyncKind counts"
    );
}

#[test]
fn recovery_invariant_all_tiers_x_ingest_seal_publish() {
    for tier in [CommitTier::None, CommitTier::Ordered, CommitTier::Durable] {
        for workload in [Workload::Ingest, Workload::Seal, Workload::Publish] {
            let recorder = CrashVfs::new(MemoryVfs::new()).expect("recorder");
            let writer =
                WalWriter::create(&recorder, Path::new(WAL_PATH), LogSeq::new(1), policy(tier))
                    .expect("writer");
            for index in 0..workload.records() {
                writer
                    .commit_durable(index as u16 + 1, &[index as u8; 32])
                    .expect("commit");
            }
            let operations = recorder.operations().expect("operations");
            let states = recorder.crash_states().expect("states");
            assert!(!states.was_capped(), "{tier:?} {workload:?} was capped");
            let committed = (1..=workload.records() as u64).collect::<Vec<_>>();
            for state in states.iter() {
                let recovered = WalReader::open(state.vfs(), Path::new(WAL_PATH));
                let actual = match recovered {
                    Ok(reader) => {
                        let sequences = reader
                            .records()
                            .iter()
                            .map(|record| record.seq.get())
                            .collect::<Vec<_>>();
                        let is_prefix = committed.starts_with(&sequences);
                        let required = if tier == CommitTier::Durable {
                            completed_full_groups(state.kind(), &operations)
                        } else {
                            0
                        };
                        if is_prefix && sequences.len() >= required {
                            "prefix".to_owned()
                        } else {
                            format!("non-prefix {sequences:?}, required durable groups {required}")
                        }
                    }
                    Err(error) => format!("recovery error requiring manual steps: {error}"),
                };
                assert_eq!(
                    actual,
                    "prefix",
                    "tier={tier:?} workload={workload:?} state={:?}",
                    state.kind()
                );
            }
            eprintln!(
                "wal_recovery tier={tier:?} workload={workload:?} states={} committed={}",
                states.len(),
                committed.len()
            );
        }
    }
}

#[test]
fn real_durable_log_refuses_a_snapshot_ahead_of_replay() {
    let store = MemoryVfs::new();
    let writer = WalWriter::create(
        &store,
        Path::new(WAL_PATH),
        LogSeq::new(1),
        policy(CommitTier::Durable),
    )
    .expect("writer");
    writer.commit_durable(1, b"one").expect("commit");
    let reader = WalReader::open(&store, Path::new(WAL_PATH)).expect("reader");
    assert_eq!(reader.durable_end(), 1);

    let manifest = Manifest {
        generation: 1,
        log_seq: 2,
        segments: Vec::new(),
        epochs: Vec::new(),
        epoch_alias: None,
        schema: Schema::new(Vec::new()).expect("schema"),
    };
    let path = Path::new("/wal-recovery").join(MANIFEST_FILE);
    store
        .insert(&path, encode_manifest(&manifest).expect("manifest bytes"))
        .expect("manifest");
    let actual = load_manifest(&store, &path, reader.durable_end())
        .map(|_| "accepted".to_owned())
        .unwrap_or_else(|error| match error {
            ManifestError::AheadOfLog { snapshot, durable } => {
                format!("AheadOfLog({snapshot}, {durable})")
            }
            other => format!("unexpected {other}"),
        });
    assert_eq!(actual, "AheadOfLog(2, 1)");
}

#[cfg(unix)]
fn parse_child_tier(value: &str) -> CommitTier {
    match value {
        "none" => CommitTier::None,
        "ordered" => CommitTier::Ordered,
        "durable" => CommitTier::Durable,
        other => panic!("unknown child tier {other}"),
    }
}

#[cfg(unix)]
fn tier_name(tier: CommitTier) -> &'static str {
    match tier {
        CommitTier::None => "none",
        CommitTier::Ordered => "ordered",
        CommitTier::Durable => "durable",
    }
}

#[cfg(unix)]
fn flock(file: &std::fs::File, operation: libc::c_int) -> std::io::Result<()> {
    let result = unsafe {
        // SAFETY: `file` owns a live descriptor for the duration of the call.
        libc::flock(file.as_raw_fd(), operation)
    };
    if result == -1 {
        Err(std::io::Error::last_os_error())
    } else {
        Ok(())
    }
}

#[cfg(unix)]
#[derive(Clone, Copy)]
struct KillPointVfs {
    threshold: u64,
}

#[cfg(unix)]
struct KillPointVfsFile {
    inner: Box<dyn VfsFile>,
    path: PathBuf,
    threshold: u64,
}

#[cfg(unix)]
impl VfsFile for KillPointVfsFile {
    fn append(&mut self, bytes: &[u8]) -> std::io::Result<()> {
        for byte in bytes {
            self.inner.append(std::slice::from_ref(byte))?;
            if std::fs::metadata(&self.path)?.len() > self.threshold {
                loop {
                    thread::yield_now();
                }
            }
        }
        Ok(())
    }

    fn sync(&self, kind: SyncKind) -> std::io::Result<()> {
        self.inner.sync(kind)
    }
}

#[cfg(unix)]
impl Vfs for KillPointVfs {
    fn ensure_directory(&self, path: &Path, create: bool) -> std::io::Result<bool> {
        StdVfs.ensure_directory(path, create)
    }

    fn open(&self, path: &Path) -> std::io::Result<u64> {
        StdVfs.open(path)
    }

    fn open_for_map(&self, path: &Path) -> std::io::Result<std::fs::File> {
        StdVfs.open_for_map(path)
    }

    fn read(&self, path: &Path) -> std::io::Result<Vec<u8>> {
        StdVfs.read(path)
    }

    fn read_range(&self, path: &Path, offset: u64, length: usize) -> std::io::Result<Vec<u8>> {
        StdVfs.read_range(path, offset, length)
    }

    fn write(&self, path: &Path, bytes: &[u8]) -> std::io::Result<()> {
        StdVfs.write(path, bytes)
    }

    fn open_append(&self, path: &Path) -> std::io::Result<Box<dyn VfsFile>> {
        Ok(Box::new(KillPointVfsFile {
            inner: StdVfs.open_append(path)?,
            path: path.to_path_buf(),
            threshold: self.threshold,
        }))
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

#[cfg(unix)]
fn read_acknowledged_sequences(path: &Path) -> Vec<u64> {
    let bytes = std::fs::read(path).expect("read child progress");
    assert_eq!(
        bytes.len() % size_of::<u64>(),
        0,
        "child progress ended with a partial sequence"
    );
    bytes
        .chunks_exact(size_of::<u64>())
        .map(|raw| u64::from_le_bytes(raw.try_into().expect("progress sequence width")))
        .collect()
}

#[test]
#[ignore = "spawned only by kill9_recovery_requires_zero_manual_cleanup"]
#[cfg(unix)]
fn child_process() {
    let directory = PathBuf::from(std::env::var_os("ZE_KILL9_CHILD_DIR").expect("child dir"));
    let tier = parse_child_tier(&std::env::var("ZE_KILL9_CHILD_TIER").expect("child tier"));
    let threshold = std::env::var("ZE_KILL9_THRESHOLD")
        .expect("kill threshold")
        .parse::<u64>()
        .expect("numeric kill threshold");
    let _store_lock = StoreLock::acquire(&directory).expect("lock writer fd");
    let vfs = KillPointVfs { threshold };
    let writer = WalWriter::create(
        &vfs,
        &directory.join("wal.ze"),
        LogSeq::new(1),
        policy(tier),
    )
    .expect("child writer");
    let mut progress = OpenOptions::new()
        .create(true)
        .append(true)
        .open(directory.join("progress"))
        .expect("child progress file");
    std::fs::write(directory.join("ready"), b"ready").expect("ready marker");

    let mut sequence = 1_u64;
    loop {
        let acknowledged = writer
            .commit_durable(1, &sequence.to_le_bytes())
            .expect("child commit");
        progress
            .write_all(&acknowledged.get().to_le_bytes())
            .expect("record acknowledged sequence");
        sequence = sequence.saturating_add(1);
    }
}

#[test]
#[ignore = "real-process SIGKILL loop"]
#[cfg(unix)]
fn kill9_recovery_requires_zero_manual_cleanup() {
    let iterations = std::env::var("ZE_KILL9_ITERS")
        .ok()
        .and_then(|value| value.parse::<usize>().ok())
        .unwrap_or(6);
    let tiers = [CommitTier::None, CommitTier::Ordered, CommitTier::Durable];
    let mut rng = ChaCha8Rng::seed_from_u64(0x0008_b2b0_0000_0001);
    let mut manual_cleanup = 0_usize;
    let mut recovered_counts = BTreeMap::new();
    let mut recovered_counts_by_tier = BTreeMap::new();

    const PAYLOAD_BYTES: usize = size_of::<u64>();
    const RECORD_BYTES: usize = MIN_RECORD_LEN + PAYLOAD_BYTES;

    for iteration in 0..iterations {
        let tier = tiers[iteration % tiers.len()];
        let records_before_torn_tail = rng.random_range(1..=16_u64);
        let interior_offset = rng.random_range(0..u64::try_from(RECORD_BYTES - 1).expect("width"));
        let kill_threshold = u64::try_from(WAL_HEADER_LEN)
            .expect("header width")
            .saturating_add(
                records_before_torn_tail
                    .saturating_mul(u64::try_from(RECORD_BYTES).expect("record width")),
            )
            .saturating_add(interior_offset);
        let directory = tempdir().expect("kill9 tempdir");
        let mut child = Command::new(std::env::current_exe().expect("test executable"))
            .args(["--exact", "child_process", "--ignored"])
            .env("ZE_KILL9_CHILD_DIR", directory.path())
            .env("ZE_KILL9_CHILD_TIER", tier_name(tier))
            .env("ZE_KILL9_THRESHOLD", kill_threshold.to_string())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .expect("spawn child");

        let ready = directory.path().join("ready");
        loop {
            if ready.exists() {
                break;
            }
            if let Some(status) = child.try_wait().expect("poll child") {
                panic!("child exited before SIGKILL: {status}");
            }
            thread::yield_now();
        }

        let wal_path = directory.path().join("wal.ze");
        loop {
            match std::fs::metadata(&wal_path) {
                Ok(metadata) if metadata.len() > kill_threshold => break,
                Ok(_) => {}
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                Err(error) => panic!("iteration={iteration} tier={tier:?} poll WAL: {error}"),
            }
            if let Some(status) = child.try_wait().expect("poll child") {
                panic!("child exited before WAL threshold: {status}");
            }
            thread::yield_now();
        }
        let killed = unsafe {
            // SAFETY: the pid belongs to the live child process above.
            libc::kill(child.id() as libc::pid_t, libc::SIGKILL)
        };
        assert_eq!(killed, 0, "iteration={iteration} tier={tier:?} SIGKILL");
        let status = child.wait().expect("wait child");
        assert_eq!(
            status.signal(),
            Some(libc::SIGKILL),
            "iteration={iteration} tier={tier:?} child status"
        );

        let lock = OpenOptions::new()
            .read(true)
            .write(true)
            .open(directory.path().join(STORE_LOCK_FILE))
            .expect("reopen stale lock path");
        if flock(&lock, libc::LOCK_EX | libc::LOCK_NB).is_err() {
            manual_cleanup = manual_cleanup.saturating_add(1);
        }
        let acknowledged = read_acknowledged_sequences(&directory.path().join("progress"));
        assert!(
            acknowledged
                .iter()
                .copied()
                .eq(1..=acknowledged.len() as u64),
            "iteration={iteration} tier={tier:?} child acknowledged non-prefix {acknowledged:?}"
        );
        let reader =
            WalReader::open(&StdVfs, &wal_path).expect("reopen without manual WAL cleanup");
        let sequences = reader
            .records()
            .iter()
            .map(|record| record.seq.get())
            .collect::<Vec<_>>();
        *recovered_counts.entry(sequences.len()).or_insert(0_usize) += 1;
        *recovered_counts_by_tier
            .entry(tier_name(tier))
            .or_insert_with(BTreeMap::new)
            .entry(sequences.len())
            .or_insert(0_usize) += 1;
        assert!(
            !sequences.is_empty(),
            "iteration={iteration} tier={tier:?} threshold after an acknowledged record recovered no records"
        );
        assert!(
            sequences.iter().copied().eq(1..=sequences.len() as u64),
            "iteration={iteration} tier={tier:?} recovered non-prefix {sequences:?}"
        );
        assert!(
            sequences.len() <= acknowledged.len()
                && sequences
                    .iter()
                    .copied()
                    .eq(acknowledged.iter().copied().take(sequences.len())),
            "iteration={iteration} tier={tier:?} recovered {sequences:?} is not a prefix of acknowledged {acknowledged:?}"
        );
        flock(&lock, libc::LOCK_UN).expect("unlock parent fd");
    }

    eprintln!(
        "kill9 iterations={iterations} manual_cleanup={manual_cleanup} recovered_count_distribution={recovered_counts:?} recovered_count_distribution_by_tier={recovered_counts_by_tier:?}"
    );
    assert_eq!(manual_cleanup, 0, "stale flock required manual cleanup");
}

#[test]
#[cfg(unix)]
fn store_lock_rejects_a_second_writer_and_releases_on_drop() {
    let directory = tempdir().expect("lock tempdir");
    let first = StoreLock::acquire(directory.path()).expect("first writer");
    let second = StoreLock::acquire(directory.path()).expect_err("second writer rejected");
    let actual = match second {
        zeppelin_embed::lifecycle::lock::StoreLockError::Io { source, .. } => source.kind(),
    };
    assert_eq!(actual, std::io::ErrorKind::WouldBlock);
    drop(first);
    StoreLock::acquire(directory.path()).expect("kernel released lock on close");
}

#[test]
#[ignore = "local evidence run only"]
fn evidence_ingest_throughput_per_tier() {
    const THREADS: usize = 16;
    const DOCUMENTS: usize = 256;
    const PAYLOAD_BYTES: usize = 1_024;

    for tier in [CommitTier::None, CommitTier::Ordered, CommitTier::Durable] {
        let directory = tempdir().expect("throughput tempdir");
        let writer = Arc::new(
            WalWriter::create(
                &StdVfs,
                &directory.path().join("wal.ze"),
                LogSeq::new(1),
                policy(tier),
            )
            .expect("writer"),
        );
        let start_gate = Arc::new(Barrier::new(THREADS + 1));
        let workers = (0..THREADS)
            .map(|worker| {
                let worker_writer = Arc::clone(&writer);
                let worker_gate = Arc::clone(&start_gate);
                thread::spawn(move || {
                    let payload = [worker as u8; PAYLOAD_BYTES];
                    worker_gate.wait();
                    for _ in 0..DOCUMENTS / THREADS {
                        worker_writer
                            .commit_durable(1, &payload)
                            .expect("throughput commit");
                    }
                })
            })
            .collect::<Vec<_>>();
        let started = Instant::now();
        start_gate.wait();
        for worker in workers {
            worker.join().expect("throughput worker");
        }
        let elapsed = started.elapsed();
        let stats = writer.stats().expect("stats");
        let docs_per_second = DOCUMENTS as f64 / elapsed.as_secs_f64();
        let mut distribution = BTreeMap::new();
        for group in &stats.recent_groups {
            *distribution.entry(group.records).or_insert(0_usize) += 1;
        }
        let group_sizes = stats
            .recent_groups
            .iter()
            .map(|group| group.records)
            .collect::<Vec<_>>();
        eprintln!(
            "wal_ingest tier={tier:?} documents={DOCUMENTS} payload_bytes={PAYLOAD_BYTES} threads={THREADS} elapsed_ns={} docs_per_second={docs_per_second:.3} flushes={} group_size_distribution={distribution:?} group_sizes={:?}",
            elapsed.as_nanos(),
            stats.completed_groups,
            group_sizes
        );
        assert_eq!(
            writer
                .visible_records(LogSeq::new(1), DOCUMENTS)
                .expect("visible")
                .len(),
            DOCUMENTS
        );
        assert_eq!(stats.durable_end, Some(LogSeq::new(DOCUMENTS as u64)));
    }
}

#[test]
fn none_loses_tail_under_power_cut_and_nothing_under_app_crash() {
    let fault = FaultVfs::new();
    let writer = WalWriter::create(
        &fault,
        Path::new(WAL_PATH),
        LogSeq::new(1),
        policy(CommitTier::None),
    )
    .expect("writer");
    writer.commit_durable(1, b"visible").expect("commit");

    assert_eq!(
        recovered_sequences(&fault.application_crash().expect("app crash")),
        vec![1],
        "none must retain page-cache bytes across an application crash"
    );
    assert_eq!(
        recovered_sequences(&fault.power_cut().expect("power cut")),
        Vec::<u64>::new(),
        "none must be able to lose its unsynchronized tail on power loss"
    );
}

#[test]
fn ordered_never_recovers_out_of_order_and_may_lose_the_tail() {
    let fault = FaultVfs::new();
    let writer = WalWriter::create(
        &fault,
        Path::new(WAL_PATH),
        LogSeq::new(1),
        policy(CommitTier::Ordered),
    )
    .expect("writer");
    writer.commit_durable(1, b"one").expect("first commit");
    writer.commit_durable(1, b"two").expect("second commit");

    let recovered = recovered_sequences(&fault.power_cut().expect("power cut"));
    assert_eq!(
        recovered,
        Vec::<u64>::new(),
        "scripted power loss drops the ordered tail without exposing a later record"
    );
}

#[test]
fn durable_retains_every_group_whose_flush_returned() {
    let fault = FaultVfs::new();
    let writer = WalWriter::create(
        &fault,
        Path::new(WAL_PATH),
        LogSeq::new(1),
        policy(CommitTier::Durable),
    )
    .expect("writer");
    writer.commit_durable(1, b"one").expect("first commit");
    writer.commit_durable(1, b"two").expect("second commit");

    assert_eq!(
        recovered_sequences(&fault.power_cut().expect("power cut")),
        vec![1, 2],
        "full-sync groups that returned must survive power loss"
    );
}

#[test]
fn durable_follower_waits_for_its_ordered_or_full_sync_to_return() {
    for tier in [CommitTier::Ordered, CommitTier::Durable] {
        let blocking = BlockingVfs::new(FaultVfs::new());
        blocking.block_next_syncs(2).expect("arm two syncs");
        let writer = Arc::new(
            WalWriter::create(&blocking, Path::new(WAL_PATH), LogSeq::new(1), policy(tier))
                .expect("writer"),
        );

        let leader_writer = Arc::clone(&writer);
        let leader = thread::spawn(move || leader_writer.commit_durable(1, b"leader"));
        blocking.wait_until_blocked(1).expect("first sync blocked");

        let follower_returned = Arc::new(AtomicBool::new(false));
        let follower_writer = Arc::clone(&writer);
        let follower_flag = Arc::clone(&follower_returned);
        let follower = thread::spawn(move || {
            let result = follower_writer.commit_durable(1, b"follower");
            follower_flag.store(true, Ordering::Release);
            result
        });
        while writer
            .visible_records(LogSeq::new(1), 2)
            .expect("visible records")
            .len()
            < 2
        {
            thread::yield_now();
        }

        blocking.release_syncs(1).expect("release first sync");
        blocking.wait_until_blocked(2).expect("second sync blocked");
        assert!(
            !follower_returned.load(Ordering::Acquire),
            "{tier:?} follower commit_durable returned before its sync returned"
        );

        blocking.release_syncs(1).expect("release second sync");
        leader
            .join()
            .expect("leader thread")
            .expect("leader commit");
        follower
            .join()
            .expect("follower thread")
            .expect("follower commit");
        assert!(
            follower_returned.load(Ordering::Acquire),
            "{tier:?} follower did not return after its sync returned"
        );
    }
}

#[test]
fn n_appends_produce_ceil_n_over_group_flushes() {
    let blocking = BlockingVfs::new(FaultVfs::new());
    blocking.block_next_syncs(1).expect("arm first barrier");
    let counting = CountingVfs::new(blocking.clone());
    let writer = Arc::new(
        WalWriter::create_with_max_group_bytes(
            &counting,
            Path::new(WAL_PATH),
            LogSeq::new(1),
            policy(CommitTier::Ordered),
            128,
        )
        .expect("writer"),
    );

    let leader_writer = Arc::clone(&writer);
    let leader = thread::spawn(move || leader_writer.commit_durable(7, &[0; 10]));
    blocking
        .wait_until_blocked(1)
        .expect("first barrier blocked");

    let followers = (0..4)
        .map(|_| {
            let follower_writer = Arc::clone(&writer);
            thread::spawn(move || follower_writer.commit(7, &[0; 10]))
        })
        .collect::<Vec<_>>();
    for follower in followers {
        follower
            .join()
            .expect("follower thread")
            .expect("visible commit");
    }
    assert_eq!(
        writer
            .visible_records(LogSeq::new(1), 5)
            .expect("visible records")
            .len(),
        5,
        "followers must become visible while the first barrier is parked"
    );

    blocking.release_syncs(1).expect("release first barrier");
    leader
        .join()
        .expect("leader thread")
        .expect("durable commit");

    let stats = writer.stats().expect("stats");
    eprintln!(
        "wal_group_commit commits=5 max_group_bytes=128 appends={} barrier={} full={} group_sizes={:?} group_bytes={:?}",
        counting.append_calls(),
        counting.handle_barrier_sync_calls(),
        counting.handle_full_sync_calls(),
        stats
            .recent_groups
            .iter()
            .map(|group| group.records)
            .collect::<Vec<_>>(),
        stats
            .recent_groups
            .iter()
            .map(|group| group.encoded_bytes)
            .collect::<Vec<_>>()
    );
    let group_sizes = stats
        .recent_groups
        .iter()
        .map(|group| group.records)
        .collect::<Vec<_>>();
    let group_bytes = stats
        .recent_groups
        .iter()
        .map(|group| group.encoded_bytes)
        .collect::<Vec<_>>();
    assert_eq!(
        (
            counting.append_calls(),
            counting.handle_barrier_sync_calls(),
            counting.handle_full_sync_calls(),
            group_sizes,
            group_bytes,
            stats.pending_records,
        ),
        (2, 2, 0, vec![1, 4], vec![72, 128], 0),
        "five commits with a four-record byte cap must produce exactly ceil(5/4) groups"
    );
}

#[test]
fn flush_waits_for_records_staged_behind_an_in_flight_sync() {
    let blocking = BlockingVfs::new(FaultVfs::new());
    blocking.block_next_syncs(1).expect("arm first barrier");
    let writer = Arc::new(
        WalWriter::create(
            &blocking,
            Path::new(WAL_PATH),
            LogSeq::new(1),
            policy(CommitTier::Ordered),
        )
        .expect("writer"),
    );

    let leader_writer = Arc::clone(&writer);
    let leader = thread::spawn(move || leader_writer.commit_durable(7, b"leader"));
    blocking
        .wait_until_blocked(1)
        .expect("first barrier blocked");
    assert_eq!(
        writer.commit(7, b"follower").expect("visible follower"),
        LogSeq::new(2)
    );

    let flushing_writer = Arc::clone(&writer);
    let flush = thread::spawn(move || flushing_writer.flush());
    blocking.release_syncs(1).expect("release first barrier");

    leader
        .join()
        .expect("leader thread")
        .expect("durable leader");
    flush
        .join()
        .expect("flush thread")
        .expect("quiescent writer");
    let stats = writer.stats().expect("stats");
    assert_eq!(
        (
            stats.completed_records,
            stats.pending_records,
            stats.durable_end
        ),
        (2, 0, Some(LogSeq::new(2))),
        "flush must return only after every record visible at its call is completed"
    );
}

fn sealed_recovery_epoch() -> StoreEpoch {
    let document = EmbeddingTower {
        model_id: "wal-sealed-recovery".to_owned(),
        model_version: "1".to_owned(),
        weights_digest: vec![0x09],
        dims: SEALED_RECOVERY_DIMS as u32,
        normalization: Normalization::None,
        prompt_prefix: String::new(),
        max_tokens: 512,
        runtime: EmbeddingRuntime::CpuReference,
        compute_units: ComputeUnits::Cpu,
        os_build: None,
    };
    StoreEpoch {
        embedding: EmbeddingEpoch {
            query: document.clone(),
            document,
            alignment_digest: Vec::new(),
        },
        tokenizer: TokenizerConfig::text_default().epoch(),
    }
}

fn sealed_recovery_vector(row: usize) -> Vec<f32> {
    let mut vector = vec![0.0; SEALED_RECOVERY_DIMS];
    vector[0] = row as f32;
    vector[1] = (row.saturating_mul(row)) as f32;
    vector
}

fn maintain_sealed_recovery_graph(store: &Store) -> zeppelin_embed::tier::MaintenanceReport {
    store.maintain_with_test_thresholds(
        MaintenanceBudget {
            wall_time: Duration::from_secs(30),
            bytes: u64::MAX,
        },
        TierThresholds {
            graph_min_rows: SEALED_RECOVERY_ROWS as u32,
        },
    )
}

#[test]
fn wal_committed_delete_of_sealed_document_stays_gone_after_manifest_commit_crash() {
    let directory = tempdir().expect("sealed-delete recovery directory");
    let epoch = sealed_recovery_epoch();
    let vfs = Arc::new(FailNextManifestRenameVfs::new());
    let dependencies = StoreTestDependencies::new(vfs.clone(), Arc::new(SystemMonotonicClock));
    let store = Store::open_with_test_dependencies(
        directory.path(),
        StoreOpenOptions::default().with_epoch(epoch.clone()),
        dependencies,
    )
    .expect("open sealed-delete recovery Store");
    let documents = (0..SEALED_RECOVERY_ROWS)
        .map(|row| {
            IngestDocument::new(
                DocumentVersion::new(DocId::new((row + 1) as u128), Revision::new(1)),
                sealed_recovery_vector(row),
            )
            .with_timestamp(100 + row as i64)
        })
        .collect::<Vec<_>>();
    store
        .ingest(IngestBatch::new(documents).with_epoch(epoch.identity()))
        .expect("ingest sealed-delete recovery fixture");
    store.seal().expect("seal delete recovery fixture");
    let maintenance = maintain_sealed_recovery_graph(&store);
    assert!(matches!(maintenance.status, MaintenanceStatus::Complete));
    assert_eq!(maintenance.graphs_built, 1);

    let deleted = DocId::new(4);
    let wal_path = directory.path().join("wal.ze");
    let wal_before = std::fs::metadata(&wal_path)
        .expect("WAL before sealed delete")
        .len();
    vfs.arm();
    let error = store
        .delete(DeleteBatch::new(vec![deleted]))
        .expect_err("manifest commit fault must fail sealed delete");
    match error {
        IngestError::Store(StoreError::Manifest(ManifestError::Io { source, .. })) => {
            assert_eq!(source.raw_os_error(), Some(libc::EIO));
        }
        other => panic!("unexpected sealed-delete failure: {other}"),
    }
    assert!(!vfs.is_armed(), "manifest rename fault did not fire");
    let wal_after = std::fs::metadata(&wal_path)
        .expect("WAL after sealed delete")
        .len();
    assert!(
        wal_after > wal_before,
        "delete WAL record was not durable before manifest failure"
    );
    store.close().expect("close sealed-delete crash state");

    let read_only_error = match Store::open(
        directory.path(),
        StoreOpenOptions::read_only().with_epoch(epoch.clone()),
    ) {
        Ok(read_only) => {
            read_only.close().expect("close unsafe read-only Store");
            panic!("read-only open served an unreconciled sealed tombstone");
        }
        Err(error) => error,
    };
    assert!(matches!(
        read_only_error,
        StoreError::SealedTombstoneRecoveryRequired
    ));

    let reopened = Store::open(
        directory.path(),
        StoreOpenOptions::default().with_epoch(epoch),
    )
    .expect("reopen sealed-delete crash state");
    let maintenance = maintain_sealed_recovery_graph(&reopened);
    assert!(matches!(maintenance.status, MaintenanceStatus::Complete));
    let query = sealed_recovery_vector(3);
    let graph = reopened
        .search(
            SearchRequest::new(&query),
            SEALED_RECOVERY_ROWS,
            SearchOptions::default().with_tier(SearchTier::Graph(GraphSearchOptions::new(
                GraphSearchProfile::SiftClass,
            ))),
            QueryControl::Cancel(CancelToken::new()),
        )
        .expect("graph search after sealed-delete recovery");
    assert!(
        graph
            .candidates
            .iter()
            .filter_map(|candidate| candidate.document())
            .all(|version| version.doc_id() != deleted),
        "deleted document was resurrected by graph search"
    );
    let exact = reopened
        .search(
            SearchRequest::new(&query),
            SEALED_RECOVERY_ROWS,
            SearchOptions::default().with_tier(SearchTier::Exact),
            QueryControl::Cancel(CancelToken::new()),
        )
        .expect("exact search after sealed-delete recovery");
    assert!(
        exact
            .candidates
            .iter()
            .filter_map(|candidate| candidate.document())
            .all(|version| version.doc_id() != deleted),
        "deleted document was resurrected by exact search"
    );
    let filtered = reopened
        .search_filtered(
            SearchRequest::new(&query),
            &Predicate::Eq {
                column: TIMESTAMP_COLUMN,
                value: PredicateValue::I64(103),
            },
            SEALED_RECOVERY_ROWS,
            SearchOptions::default().with_tier(SearchTier::Exact),
            QueryControl::Cancel(CancelToken::new()),
        )
        .expect("timestamp search after sealed-delete recovery");
    assert!(
        filtered
            .candidates
            .iter()
            .filter_map(|candidate| candidate.document())
            .all(|version| version.doc_id() != deleted),
        "deleted document was resurrected by timestamp search"
    );
    reopened.close().expect("close recovered delete Store");
}

#[test]
fn wal_committed_supersede_leaves_no_stale_sealed_revision_after_manifest_commit_crash() {
    let directory = tempdir().expect("sealed-revision recovery directory");
    let epoch = sealed_recovery_epoch();
    let vfs = Arc::new(FailNextManifestRenameVfs::new());
    let dependencies = StoreTestDependencies::new(vfs.clone(), Arc::new(SystemMonotonicClock));
    let store = Store::open_with_test_dependencies(
        directory.path(),
        StoreOpenOptions::default().with_epoch(epoch.clone()),
        dependencies,
    )
    .expect("open sealed-revision recovery Store");
    let documents = (0..SEALED_RECOVERY_ROWS)
        .map(|row| {
            IngestDocument::new(
                DocumentVersion::new(DocId::new((row + 1) as u128), Revision::new(1)),
                sealed_recovery_vector(row),
            )
            .with_timestamp(100 + row as i64)
        })
        .collect::<Vec<_>>();
    store
        .ingest(IngestBatch::new(documents).with_epoch(epoch.identity()))
        .expect("ingest sealed-revision recovery fixture");
    store.seal().expect("seal revision recovery fixture");
    let maintenance = maintain_sealed_recovery_graph(&store);
    assert!(matches!(maintenance.status, MaintenanceStatus::Complete));
    assert_eq!(maintenance.graphs_built, 1);

    let revised = DocId::new(4);
    let revised_version = DocumentVersion::new(revised, Revision::new(2));
    let revised_vector = sealed_recovery_vector(20);
    let wal_path = directory.path().join("wal.ze");
    let wal_before = std::fs::metadata(&wal_path)
        .expect("WAL before sealed revision")
        .len();
    vfs.arm();
    let error = store
        .ingest(
            IngestBatch::new(vec![
                IngestDocument::new(revised_version, revised_vector.clone()).with_timestamp(203),
            ])
            .with_epoch(epoch.identity()),
        )
        .expect_err("manifest commit fault must fail sealed revision");
    match error {
        IngestError::Store(StoreError::Manifest(ManifestError::Io { source, .. })) => {
            assert_eq!(source.raw_os_error(), Some(libc::EIO));
        }
        other => panic!("unexpected sealed-revision failure: {other}"),
    }
    assert!(!vfs.is_armed(), "manifest rename fault did not fire");
    let wal_after = std::fs::metadata(&wal_path)
        .expect("WAL after sealed revision")
        .len();
    assert!(
        wal_after > wal_before,
        "revision WAL record was not durable before manifest failure"
    );
    store.close().expect("close sealed-revision crash state");

    let reopened = Store::open(
        directory.path(),
        StoreOpenOptions::default().with_epoch(epoch),
    )
    .expect("reopen sealed-revision crash state");
    let exact = reopened
        .search(
            SearchRequest::new(&revised_vector),
            SEALED_RECOVERY_ROWS + 1,
            SearchOptions::default().with_tier(SearchTier::Exact),
            QueryControl::Cancel(CancelToken::new()),
        )
        .expect("exact search after sealed-revision recovery");
    let returned_revisions = exact
        .candidates
        .iter()
        .filter_map(|candidate| candidate.document())
        .filter(|version| version.doc_id() == revised)
        .map(DocumentVersion::revision)
        .collect::<Vec<_>>();
    assert_eq!(
        returned_revisions,
        vec![Revision::new(2)],
        "superseded sealed revision was resurrected"
    );
    reopened.close().expect("close recovered revision Store");
}

// Keep the path types imported above available to the standalone test-support
// module, whose `super` is this integration-test crate.
const _: Option<PathBuf> = None;
const _: Option<SyncKind> = None;
fn _vfs_file_type(_: Option<Box<dyn VfsFile>>) {}
