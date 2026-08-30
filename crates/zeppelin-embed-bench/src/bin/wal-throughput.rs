//! Sustained-offered-load WAL throughput harness.

use std::error::Error;
use std::io::{self, IoSlice};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU8, AtomicU64, Ordering};
use std::sync::{Arc, Barrier, OnceLock};
use std::thread;
use std::time::{Duration, Instant};

use tempfile::tempdir;
use zeppelin_embed::lifecycle::durability::{CommitTier, DurabilityMode, DurabilityPolicy};
use zeppelin_embed::manifest::io::DurableLog;
use zeppelin_embed::vfs::{CountingVfs, StdVfs, SyncKind, Vfs, VfsFile};
use zeppelin_embed::wal::header::WAL_HEADER_LEN;
use zeppelin_embed::wal::record::MIN_RECORD_LEN;
use zeppelin_embed::wal::{
    DEFAULT_MAX_GROUP_BYTES, GROUP_SIZE_HISTOGRAM_BUCKETS, GroupSizeHistogram, LogSeq, WalWriter,
};
#[cfg(test)]
use zeppelin_embed_bench::platform::taint::{Taint, evaluate_taint};
use zeppelin_embed_bench::platform::taint::{
    TaintCheck, detect_taint, format_load1, format_taint_labels, print_taint_status,
};

const DEFAULT_CELL_SECONDS: u64 = 3;
const DEFAULT_CELL_BYTES: u64 = 2 * 1_024 * 1_024 * 1_024;
const RETIRE_EVERY_RECORDS: u64 = 1_024;
const BOUND_TIME: u8 = 1;
const BOUND_BYTES: u8 = 2;

fn main() {
    if let Err(error) = run() {
        eprintln!("wal-throughput: {error}");
        std::process::exit(1);
    }
}

fn run() -> Result<(), Box<dyn Error>> {
    let arguments = std::env::args().skip(1).collect::<Vec<_>>();
    let config = Config::parse(&arguments)?;
    let taint = detect_taint(config.load_limit);
    if config.smoke {
        println!("wal-throughput smoke mode");
    }
    print_taint_status(&taint, config.load_limit, "throughput");
    println!(
        "config payloads={:?} threads={:?} batches={:?} tiers={} cell_seconds={} cell_bytes={} max_group_bytes={} load_limit={}",
        config.payloads,
        config.threads,
        config.batches,
        format_tiers(&config.tiers),
        config.cell_duration.as_secs_f64(),
        config.cell_bytes,
        config.max_group_bytes,
        config.load_limit,
    );

    let mut results = Vec::new();
    for &payload_bytes in &config.payloads {
        for &threads in &config.threads {
            for &tier in &config.tiers {
                for &batch_size in &config.batches {
                    let result = run_cell(&config, payload_bytes, threads, tier, batch_size)?;
                    print_machine_line(&result, &taint);
                    results.push(result);
                }
            }
        }
    }
    print_table(&results, &taint);
    if config.smoke {
        println!("wal-throughput smoke: ok");
    }
    Ok(())
}

#[derive(Clone, Debug, PartialEq)]
struct Config {
    payloads: Vec<usize>,
    threads: Vec<usize>,
    batches: Vec<usize>,
    tiers: Vec<CommitTier>,
    cell_duration: Duration,
    cell_bytes: u64,
    max_group_bytes: usize,
    load_limit: f64,
    smoke: bool,
}

impl Config {
    fn full() -> Self {
        Self {
            payloads: vec![256, 1_024, 4_096],
            threads: vec![1, 4, 12, 16],
            batches: vec![1],
            tiers: vec![CommitTier::None, CommitTier::Ordered, CommitTier::Durable],
            cell_duration: Duration::from_secs(DEFAULT_CELL_SECONDS),
            cell_bytes: DEFAULT_CELL_BYTES,
            max_group_bytes: DEFAULT_MAX_GROUP_BYTES,
            load_limit: 1.0,
            smoke: false,
        }
    }

    fn smoke() -> Self {
        Self {
            payloads: vec![1_024],
            threads: vec![4],
            batches: vec![1],
            tiers: vec![CommitTier::None, CommitTier::Ordered, CommitTier::Durable],
            cell_duration: Duration::from_millis(75),
            cell_bytes: 4 * 1_024 * 1_024,
            max_group_bytes: DEFAULT_MAX_GROUP_BYTES,
            load_limit: 1.0,
            smoke: true,
        }
    }

    fn parse(arguments: &[String]) -> Result<Self, Box<dyn Error>> {
        let mut config = if arguments.iter().any(|argument| argument == "--smoke") {
            Self::smoke()
        } else {
            Self::full()
        };
        let mut index = 0_usize;
        while index < arguments.len() {
            let flag = arguments.get(index).map(String::as_str).unwrap_or_default();
            if flag == "--smoke" {
                index = index.saturating_add(1);
                continue;
            }
            let value = arguments.get(index.saturating_add(1)).ok_or_else(usage)?;
            match flag {
                "--payloads" => config.payloads = parse_usize_list(value, "payloads")?,
                "--threads" => config.threads = parse_usize_list(value, "threads")?,
                "--batch" => config.batches = parse_usize_list(value, "batch")?,
                "--tiers" => config.tiers = parse_tiers(value)?,
                "--cell-seconds" => {
                    let seconds = parse_u64(value, "cell-seconds")?;
                    if seconds == 0 {
                        return Err(invalid("cell-seconds must be positive"));
                    }
                    config.cell_duration = Duration::from_secs(seconds);
                }
                "--cell-bytes" => config.cell_bytes = parse_u64(value, "cell-bytes")?,
                "--max-group-bytes" => {
                    config.max_group_bytes = parse_usize(value, "max-group-bytes")?;
                }
                "--load-limit" => config.load_limit = parse_f64(value, "load-limit")?,
                _ => return Err(usage()),
            }
            index = index.saturating_add(2);
        }
        config.validate()?;
        Ok(config)
    }

    fn validate(&self) -> Result<(), Box<dyn Error>> {
        if self.payloads.is_empty()
            || self.threads.is_empty()
            || self.batches.is_empty()
            || self.tiers.is_empty()
        {
            return Err(invalid("sweep axes must not be empty"));
        }
        if self.payloads.contains(&0) || self.threads.contains(&0) || self.batches.contains(&0) {
            return Err(invalid(
                "payload, thread, and batch values must be positive",
            ));
        }
        let largest_record = self
            .payloads
            .iter()
            .copied()
            .max()
            .unwrap_or(0)
            .saturating_add(MIN_RECORD_LEN);
        if self.max_group_bytes < WAL_HEADER_LEN.saturating_add(largest_record) {
            return Err(invalid(
                "max-group-bytes must fit the WAL header plus the largest encoded record",
            ));
        }
        let largest_batch = self.batches.iter().copied().max().unwrap_or(0);
        let largest_batch_bytes = largest_record.saturating_mul(largest_batch);
        if self.cell_bytes < WAL_HEADER_LEN.saturating_add(largest_batch_bytes) as u64 {
            return Err(invalid(
                "cell-bytes must fit the WAL header plus one largest encoded batch",
            ));
        }
        Ok(())
    }
}

fn parse_usize_list(value: &str, label: &str) -> Result<Vec<usize>, Box<dyn Error>> {
    let parsed = value
        .split(',')
        .map(|item| parse_usize(item, label))
        .collect::<Result<Vec<_>, _>>()?;
    if parsed.is_empty() {
        return Err(invalid(&format!("{label} list must not be empty")));
    }
    Ok(parsed)
}

fn parse_tiers(value: &str) -> Result<Vec<CommitTier>, Box<dyn Error>> {
    value
        .split(',')
        .map(|tier| match tier {
            "none" => Ok(CommitTier::None),
            "ordered" => Ok(CommitTier::Ordered),
            "durable" => Ok(CommitTier::Durable),
            _ => Err(invalid(
                "tiers must be a comma-list of none,ordered,durable",
            )),
        })
        .collect()
}

fn parse_usize(value: &str, label: &str) -> Result<usize, Box<dyn Error>> {
    value
        .parse::<usize>()
        .map_err(|error| invalid(&format!("invalid {label} value {value:?}: {error}")))
}

fn parse_u64(value: &str, label: &str) -> Result<u64, Box<dyn Error>> {
    value
        .parse::<u64>()
        .map_err(|error| invalid(&format!("invalid {label} value {value:?}: {error}")))
}

fn parse_f64(value: &str, label: &str) -> Result<f64, Box<dyn Error>> {
    value
        .parse::<f64>()
        .map_err(|error| invalid(&format!("invalid {label} value {value:?}: {error}")))
}

fn usage() -> Box<dyn Error> {
    invalid(
        "usage: wal-throughput [--smoke] [--payloads 256,1024,4096 --threads 1,4,12,16 --batch 1 --tiers none,ordered,durable --cell-seconds 3 --cell-bytes 2147483648 --max-group-bytes 1048576 --load-limit 1.0]",
    )
}

fn invalid(message: &str) -> Box<dyn Error> {
    io::Error::new(io::ErrorKind::InvalidInput, message).into()
}

#[derive(Default)]
struct SyncTimings {
    barrier_ns: AtomicU64,
    barrier_calls: AtomicU64,
    full_ns: AtomicU64,
    full_calls: AtomicU64,
}

struct TimedVfs<V> {
    inner: V,
    timings: Arc<SyncTimings>,
}

impl<V> TimedVfs<V> {
    fn new(inner: V) -> Self {
        Self {
            inner,
            timings: Arc::new(SyncTimings::default()),
        }
    }

    fn sync_total_ns(&self, kind: SyncKind) -> u64 {
        match kind {
            SyncKind::Barrier => self.timings.barrier_ns.load(Ordering::Relaxed),
            SyncKind::Full => self.timings.full_ns.load(Ordering::Relaxed),
        }
    }

    fn sync_calls(&self, kind: SyncKind) -> u64 {
        match kind {
            SyncKind::Barrier => self.timings.barrier_calls.load(Ordering::Relaxed),
            SyncKind::Full => self.timings.full_calls.load(Ordering::Relaxed),
        }
    }
}

struct TimedVfsFile {
    inner: Box<dyn VfsFile>,
    timings: Arc<SyncTimings>,
}

impl VfsFile for TimedVfsFile {
    fn append(&mut self, bytes: &[u8]) -> io::Result<()> {
        self.inner.append(bytes)
    }

    fn append_vectored(&mut self, buffers: &mut [IoSlice<'_>]) -> io::Result<()> {
        self.inner.append_vectored(buffers)
    }

    fn sync(&self, kind: SyncKind) -> io::Result<()> {
        let started = Instant::now();
        self.inner.sync(kind)?;
        let elapsed_ns = u64::try_from(started.elapsed().as_nanos()).unwrap_or(u64::MAX);
        let (total, calls) = match kind {
            SyncKind::Barrier => (&self.timings.barrier_ns, &self.timings.barrier_calls),
            SyncKind::Full => (&self.timings.full_ns, &self.timings.full_calls),
        };
        total.fetch_add(elapsed_ns, Ordering::Relaxed);
        calls.fetch_add(1, Ordering::Relaxed);
        Ok(())
    }
}

impl<V: Vfs> Vfs for TimedVfs<V> {
    fn ensure_directory(&self, path: &Path, create: bool) -> io::Result<bool> {
        self.inner.ensure_directory(path, create)
    }

    fn open(&self, path: &Path) -> io::Result<u64> {
        self.inner.open(path)
    }

    fn open_for_map(&self, path: &Path) -> io::Result<std::fs::File> {
        self.inner.open_for_map(path)
    }

    fn read(&self, path: &Path) -> io::Result<Vec<u8>> {
        self.inner.read(path)
    }

    fn read_range(&self, path: &Path, offset: u64, length: usize) -> io::Result<Vec<u8>> {
        self.inner.read_range(path, offset, length)
    }

    fn write(&self, path: &Path, bytes: &[u8]) -> io::Result<()> {
        self.inner.write(path, bytes)
    }

    fn open_append(&self, path: &Path) -> io::Result<Box<dyn VfsFile>> {
        Ok(Box::new(TimedVfsFile {
            inner: self.inner.open_append(path)?,
            timings: Arc::clone(&self.timings),
        }))
    }

    fn rename(&self, from: &Path, to: &Path) -> io::Result<()> {
        self.inner.rename(from, to)
    }

    fn sync(&self, path: &Path, kind: SyncKind) -> io::Result<()> {
        self.inner.sync(path, kind)
    }

    fn list(&self, directory: &Path) -> io::Result<Vec<PathBuf>> {
        self.inner.list(directory)
    }

    fn delete(&self, path: &Path) -> io::Result<()> {
        self.inner.delete(path)
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum CellBound {
    Time,
    Bytes,
}

impl CellBound {
    const fn as_str(self) -> &'static str {
        match self {
            Self::Time => "time",
            Self::Bytes => "bytes",
        }
    }
}

struct CellResult {
    payload_bytes: usize,
    threads: usize,
    batch_size: usize,
    tier: CommitTier,
    bound: CellBound,
    elapsed_ns: u128,
    records: u64,
    docs_per_second: f64,
    megabytes_per_second: f64,
    flushes: u64,
    mean_group_records: f64,
    histogram: GroupSizeHistogram,
    sync_mean_ns: Option<f64>,
    implied_ceiling_docs_per_second: Option<f64>,
    achieved_fraction: Option<f64>,
    append_calls: u64,
    bytes_appended: u64,
    barrier_syncs: u64,
    full_syncs: u64,
}

fn run_cell(
    config: &Config,
    payload_bytes: usize,
    thread_count: usize,
    tier: CommitTier,
    batch_size: usize,
) -> Result<CellResult, Box<dyn Error>> {
    let directory = tempdir()?;
    let timed = TimedVfs::new(StdVfs);
    let counting = CountingVfs::new(timed);
    let writer = Arc::new(WalWriter::create_with_max_group_bytes(
        &counting,
        &directory.path().join("wal.ze"),
        LogSeq::new(1),
        policy(tier)?,
        config.max_group_bytes,
    )?);
    let start_gate = Arc::new(Barrier::new(thread_count.saturating_add(1)));
    let reserved_bytes = Arc::new(AtomicU64::new(WAL_HEADER_LEN as u64));
    let records = Arc::new(AtomicU64::new(0));
    let hit_bound = Arc::new(AtomicU8::new(0));
    let timing = Arc::new(OnceLock::<(Instant, Instant)>::new());
    let record_bytes = payload_bytes.saturating_add(MIN_RECORD_LEN) as u64;
    let batch_records =
        u64::try_from(batch_size).map_err(|_| invalid("batch size exceeds the record counter"))?;
    let batch_bytes = record_bytes
        .checked_mul(batch_records)
        .ok_or_else(|| invalid("encoded batch byte count overflowed"))?;
    let workers = (0..thread_count)
        .map(|worker| {
            let worker_writer = Arc::clone(&writer);
            let worker_gate = Arc::clone(&start_gate);
            let worker_reserved = Arc::clone(&reserved_bytes);
            let worker_records = Arc::clone(&records);
            let worker_bound = Arc::clone(&hit_bound);
            let worker_timing = Arc::clone(&timing);
            let cell_bytes = config.cell_bytes;
            thread::spawn(move || -> Result<(), String> {
                let payload = vec![worker as u8; payload_bytes];
                let batch = (0..batch_size)
                    .map(|_| (1, payload.as_slice()))
                    .collect::<Vec<_>>();
                worker_gate.wait();
                let (_, deadline) = worker_timing
                    .get()
                    .copied()
                    .ok_or_else(|| String::from("cell timing was not initialized"))?;
                loop {
                    if worker_bound.load(Ordering::Acquire) != 0 {
                        break;
                    }
                    if Instant::now() >= deadline {
                        let _ = worker_bound.compare_exchange(
                            0,
                            BOUND_TIME,
                            Ordering::AcqRel,
                            Ordering::Acquire,
                        );
                        break;
                    }
                    if !reserve_bytes(&worker_reserved, batch_bytes, cell_bytes) {
                        let _ = worker_bound.compare_exchange(
                            0,
                            BOUND_BYTES,
                            Ordering::AcqRel,
                            Ordering::Acquire,
                        );
                        break;
                    }
                    let last_sequence = if batch_size == 1 {
                        worker_writer
                            .commit(1, &payload)
                            .map_err(|error| error.to_string())?
                    } else {
                        let range = worker_writer
                            .commit_many(&batch)
                            .map_err(|error| error.to_string())?;
                        LogSeq::new(range.end.get().saturating_sub(1))
                    };
                    worker_records.fetch_add(batch_records, Ordering::Relaxed);
                    if last_sequence.get() % RETIRE_EVERY_RECORDS < batch_records {
                        retire_durable_prefix(&worker_writer).map_err(|error| error.to_string())?;
                    }
                }
                Ok(())
            })
        })
        .collect::<Vec<_>>();
    let started = Instant::now();
    timing
        .set((started, started + config.cell_duration))
        .map_err(|_| io::Error::other("cell timing initialized twice"))?;
    start_gate.wait();
    for worker in workers {
        worker
            .join()
            .map_err(|_| io::Error::other("WAL throughput producer panicked"))?
            .map_err(io::Error::other)?;
    }
    writer.flush()?;
    let elapsed = started.elapsed();
    retire_durable_prefix(&writer)?;
    let stats = writer.stats()?;
    let records = records.load(Ordering::Acquire);
    if stats.completed_records != records {
        return Err(io::Error::other(format!(
            "completed WAL records {} disagree with producer count {records}",
            stats.completed_records
        ))
        .into());
    }
    let bound = match hit_bound.load(Ordering::Acquire) {
        BOUND_TIME => CellBound::Time,
        BOUND_BYTES => CellBound::Bytes,
        _ => {
            return Err(
                io::Error::other("cell ended without observing its time or byte bound").into(),
            );
        }
    };
    let elapsed_seconds = elapsed.as_secs_f64();
    let bytes_appended = counting.bytes_appended();
    let append_calls = counting.append_calls();
    let barrier_syncs = counting.handle_barrier_sync_calls();
    let full_syncs = counting.handle_full_sync_calls();
    let (sync_total_ns, sync_calls) = match tier {
        CommitTier::None => (0, 0),
        CommitTier::Ordered => (
            counting.inner().sync_total_ns(SyncKind::Barrier),
            counting.inner().sync_calls(SyncKind::Barrier),
        ),
        CommitTier::Durable => (
            counting.inner().sync_total_ns(SyncKind::Full),
            counting.inner().sync_calls(SyncKind::Full),
        ),
    };
    if sync_calls != barrier_syncs.saturating_add(full_syncs) {
        return Err(io::Error::other(format!(
            "timed sync count {sync_calls} disagrees with deterministic sync count {}",
            barrier_syncs.saturating_add(full_syncs)
        ))
        .into());
    }
    let docs_per_second = records as f64 / elapsed_seconds;
    let sync_mean_ns = (sync_calls > 0).then(|| sync_total_ns as f64 / sync_calls as f64);
    let implied_ceiling =
        (sync_total_ns > 0).then(|| records as f64 * 1_000_000_000.0 / sync_total_ns as f64);
    let achieved_fraction = implied_ceiling.map(|ceiling| docs_per_second / ceiling);
    Ok(CellResult {
        payload_bytes,
        threads: thread_count,
        batch_size,
        tier,
        bound,
        elapsed_ns: elapsed.as_nanos(),
        records,
        docs_per_second,
        megabytes_per_second: bytes_appended as f64 / 1_000_000.0 / elapsed_seconds,
        flushes: stats.completed_groups,
        mean_group_records: stats.completed_records as f64 / stats.completed_groups as f64,
        histogram: stats.group_size_histogram,
        sync_mean_ns,
        implied_ceiling_docs_per_second: implied_ceiling,
        achieved_fraction,
        append_calls,
        bytes_appended,
        barrier_syncs,
        full_syncs,
    })
}

fn reserve_bytes(reserved: &AtomicU64, record_bytes: u64, limit: u64) -> bool {
    reserved
        .fetch_update(Ordering::AcqRel, Ordering::Acquire, |current| {
            current
                .checked_add(record_bytes)
                .filter(|next| *next <= limit)
        })
        .is_ok()
}

fn retire_durable_prefix(writer: &WalWriter) -> Result<(), Box<dyn Error>> {
    let durable_end = writer.durable_end();
    if durable_end > 0 {
        writer.retire_visible_through(LogSeq::new(durable_end))?;
    }
    Ok(())
}

fn policy(tier: CommitTier) -> Result<DurabilityPolicy, Box<dyn Error>> {
    DurabilityPolicy::new(DurabilityMode::Durable, tier).map_err(Into::into)
}

fn format_tiers(tiers: &[CommitTier]) -> String {
    tiers
        .iter()
        .map(|tier| tier_name(*tier))
        .collect::<Vec<_>>()
        .join(",")
}

const fn tier_name(tier: CommitTier) -> &'static str {
    match tier {
        CommitTier::None => "none",
        CommitTier::Ordered => "ordered",
        CommitTier::Durable => "durable",
    }
}

fn print_machine_line(result: &CellResult, taint: &TaintCheck) {
    println!(
        "WAL_THROUGHPUT payload_bytes={} threads={} batch_size={} tier={} bound={} elapsed_ns={} records={} docs_per_s={:.3} MB_per_s={:.3} flushes={} mean_group_records={:.3} group_hist={} sync_mean_ns={} implied_ceiling_docs_per_s={} achieved_fraction={} append_calls={} bytes_appended={} barrier_syncs={} full_syncs={} load1={} taint={}",
        result.payload_bytes,
        result.threads,
        result.batch_size,
        tier_name(result.tier),
        result.bound.as_str(),
        result.elapsed_ns,
        result.records,
        result.docs_per_second,
        result.megabytes_per_second,
        result.flushes,
        result.mean_group_records,
        format_histogram(&result.histogram),
        format_optional(result.sync_mean_ns),
        format_optional(result.implied_ceiling_docs_per_second),
        format_optional(result.achieved_fraction),
        result.append_calls,
        result.bytes_appended,
        result.barrier_syncs,
        result.full_syncs,
        format_load1(taint.load1),
        format_taint_labels(&taint.taints),
    );
}

fn print_table(results: &[CellResult], taint: &TaintCheck) {
    let labels = format_taint_labels(&taint.taints);
    if taint.taints.is_empty() {
        println!("\n== WAL throughput (taint: none) ==");
    } else {
        println!("\n== WAL throughput (TAINTED: {labels}) ==");
    }
    println!(
        "payload  thr  batch  tier      bound  records    docs/s       MB/s     flushes  mean-group  achieved"
    );
    for result in results {
        println!(
            "{:>7}  {:>3}  {:>5}  {:<8}  {:<5}  {:>8}  {:>11.1}  {:>9.1}  {:>7}  {:>10.2}  {:>8}",
            result.payload_bytes,
            result.threads,
            result.batch_size,
            tier_name(result.tier),
            result.bound.as_str(),
            result.records,
            result.docs_per_second,
            result.megabytes_per_second,
            result.flushes,
            result.mean_group_records,
            result
                .achieved_fraction
                .map(|value| format!("{value:.4}"))
                .unwrap_or_else(|| String::from("NA")),
        );
        println!(
            "         group-size distribution: {}",
            format_histogram(&result.histogram)
        );
    }
}

fn format_optional(value: Option<f64>) -> String {
    value
        .map(|number| format!("{number:.6}"))
        .unwrap_or_else(|| String::from("NA"))
}

fn format_histogram(histogram: &GroupSizeHistogram) -> String {
    histogram
        .buckets
        .iter()
        .enumerate()
        .filter(|(_, count)| **count > 0)
        .map(|(bucket, count)| format!("{}:{count}", histogram_label(bucket)))
        .collect::<Vec<_>>()
        .join(",")
}

fn histogram_label(bucket: usize) -> String {
    if bucket == 0 {
        return String::from("1");
    }
    let lower = 1_usize << bucket;
    if bucket == GROUP_SIZE_HISTOGRAM_BUCKETS.saturating_sub(1) {
        format!("{lower}+")
    } else {
        let upper = (1_usize << bucket.saturating_add(1)).saturating_sub(1);
        format!("{lower}-{upper}")
    }
}

#[cfg(test)]
mod tests {
    use super::{Config, Taint, evaluate_taint, histogram_label};
    use zeppelin_embed::lifecycle::durability::CommitTier;

    #[test]
    fn clean_conditions_produce_no_taint() {
        assert_eq!(evaluate_taint(Some(0.42), 1.0, false), Vec::new());
    }

    #[test]
    fn load_above_the_limit_is_tainted() {
        assert_eq!(
            evaluate_taint(Some(2.5), 1.0, false),
            vec![Taint::Load {
                actual: 2.5,
                limit: 1.0,
            }]
        );
    }

    #[test]
    fn an_unreadable_load_average_is_tainted() {
        assert_eq!(
            evaluate_taint(None, 1.0, false),
            vec![Taint::LoadUnreadable]
        );
    }

    #[test]
    fn a_sandboxed_process_is_tainted() {
        assert_eq!(evaluate_taint(Some(0.42), 1.0, true), vec![Taint::Sandbox]);
    }

    #[test]
    fn load_exactly_at_the_limit_is_clean() {
        assert_eq!(evaluate_taint(Some(1.0), 1.0, false), Vec::new());
    }

    #[test]
    fn cli_axes_and_cell_bounds_are_overridable() {
        let arguments = [
            "--payloads",
            "64,128",
            "--threads",
            "2,3",
            "--tiers",
            "none,durable",
            "--cell-seconds",
            "1",
            "--cell-bytes",
            "4096",
            "--max-group-bytes",
            "2048",
        ]
        .map(String::from);
        let config = Config::parse(&arguments).expect("valid overrides");
        assert_eq!(config.payloads, vec![64, 128]);
        assert_eq!(config.threads, vec![2, 3]);
        assert_eq!(config.tiers, vec![CommitTier::None, CommitTier::Durable]);
        assert_eq!(config.cell_duration.as_secs(), 1);
        assert_eq!(config.cell_bytes, 4_096);
        assert_eq!(config.max_group_bytes, 2_048);
    }

    #[test]
    fn cli_batch_axis_is_overridable() {
        let arguments = ["--batch", "1,64,1024,4096"].map(String::from);
        let actual = match Config::parse(&arguments) {
            Ok(config) => format!("ok={:?}", config.batches),
            Err(error) => format!("error={error}"),
        };

        assert_eq!(
            actual, "ok=[1, 64, 1024, 4096]",
            "--batch must accept the required comma-separated sweep axis"
        );
    }

    #[test]
    fn cli_load_limit_is_overridable() {
        let arguments = ["--load-limit", "2.5"].map(String::from);
        let actual = match Config::parse(&arguments) {
            Ok(config) => format!("ok={}", config.load_limit),
            Err(error) => format!("error={error}"),
        };

        assert_eq!(actual, "ok=2.5", "--load-limit must accept a number");
    }

    #[test]
    fn smoke_mode_accepts_cell_overrides() {
        let arguments =
            ["--smoke", "--cell-seconds", "1", "--cell-bytes", "4096"].map(String::from);
        let actual = match Config::parse(&arguments) {
            Ok(config) => format!(
                "smoke={} seconds={} bytes={}",
                config.smoke,
                config.cell_duration.as_secs(),
                config.cell_bytes
            ),
            Err(error) => format!("error={error}"),
        };

        assert_eq!(actual, "smoke=true seconds=1 bytes=4096");
    }

    #[test]
    fn histogram_labels_are_fixed_logarithmic_ranges() {
        assert_eq!(histogram_label(0), "1");
        assert_eq!(histogram_label(1), "2-3");
        assert_eq!(histogram_label(10), "1024-2047");
        assert_eq!(histogram_label(17), "131072+");
    }
}
