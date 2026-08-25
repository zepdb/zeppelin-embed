#![allow(clippy::expect_used, clippy::indexing_slicing)]

use std::sync::{Mutex, MutexGuard, OnceLock};
use tempfile::{TempDir, tempdir};
use zeppelin_embed::lifecycle::durability::{CommitTier, DurabilityMode, DurabilityPolicy};
use zeppelin_embed::lifecycle::{OpenOptions, Store};
use zeppelin_embed::manifest::Manifest;
use zeppelin_embed::manifest::io::commit_manifest;
use zeppelin_embed::meta::{AliveSet, ColumnStoreBuilder, Schema};
use zeppelin_embed::quant::{prepare_bit4_query, quantize_bit4};
use zeppelin_embed::scan::{ScanQuery, ScanRequest, ScanRows, top_k};
use zeppelin_embed::segment::SegmentId;
use zeppelin_embed::segment::writer::{SegmentBuild, SegmentFactors, write_segment};
use zeppelin_embed::vfs::StdVfs;

const ITERATIONS: usize = 20;
const ROWS: usize = 10_000;
const DIMS: usize = 32;
const MAX_DRIFT_BYTES: u64 = 5 * 1024 * 1024;
const RUNNER_UP_ROW: usize = 4_321;
const WINNER_ROW: usize = 7_777;

fn test_guard() -> MutexGuard<'static, ()> {
    static TEST_GUARD: OnceLock<Mutex<()>> = OnceLock::new();
    TEST_GUARD
        .get_or_init(|| Mutex::new(()))
        .lock()
        .expect("RSS test guard")
}

// This is intentionally weaker than task 09's eventual
// open -> ingest 10k docs -> query -> close gate because task 10's ingest path
// does not exist. The fixture writes 10k rows once through task 07. Every
// measured iteration opens and publishes that committed snapshot, takes a
// lease, queries mmap-backed codes/factors, drops the lease, and closes. It
// covers lifecycle/mapping/query teardown flatness; it does not cover repeated
// ingest, WAL mutation, or per-iteration sealing allocations. The non-ignored
// `repeated_close_returns_exact_stats_counters_to_pre_open_baseline` unit test
// supplies the deterministic zero-tolerance gate for the two exact counters
// that can observe leaked mappings and their anonymous bookkeeping.
#[test]
#[ignore = "macOS CI phys_footprint gate; intentionally not part of local cargo test"]
fn rss_flatness() {
    #[cfg(not(target_os = "macos"))]
    {
        eprintln!("NOT MEASURED — phys_footprint is only available on macOS");
    }

    #[cfg(target_os = "macos")]
    {
        let _guard = test_guard();
        let directory = published_fixture();
        let query_values = rss_query_values();
        let query = prepare_bit4_query(&query_values, 0x09c0).expect("prepared query");
        let load1 = read_load1();
        let sandboxed = process_is_sandboxed();
        let baseline = zeppelin_embed::sys::darwin::phys_footprint().expect("baseline footprint");
        let mut samples = [0_u64; ITERATIONS];

        for (iteration, sample) in samples.iter_mut().enumerate() {
            let store = Store::open(directory.path(), OpenOptions::default()).expect("open store");
            let lease = store.snapshot().expect("snapshot lease");
            let segment = lease.segments().first().expect("fixture segment");
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
                vec![WINNER_ROW, RUNNER_UP_ROW, 0],
                "non-degenerate mmap-backed ranking at iteration {iteration}"
            );
            drop(lease);
            store.close().expect("close store");
            *sample =
                zeppelin_embed::sys::darwin::phys_footprint().expect("post-close phys_footprint");
        }

        let final_footprint = samples[ITERATIONS - 1];
        let peak_footprint = samples.iter().copied().max().expect("non-empty samples");
        let drift = final_footprint.abs_diff(baseline);
        let peak_drift = peak_footprint.saturating_sub(baseline);
        let load_taint = load1.is_none_or(|actual| actual > 1.0);
        let taints = match (load_taint, sandboxed) {
            (false, false) => "none",
            (true, false) => "load",
            (false, true) => "sandbox",
            (true, true) => "load,sandbox",
        };
        println!(
            "RSS_FLATNESS iterations={ITERATIONS} rows={ROWS} baseline_bytes={baseline} final_bytes={final_footprint} drift_bytes={drift} peak_drift_bytes={peak_drift} load1={} load_limit=1.00 sandbox={} taints={taints} verdict={} status=PROVISIONAL",
            load1.map_or_else(|| String::from("NA"), |value| format!("{value:.2}")),
            if sandboxed { "detected" } else { "clear" },
            if drift < MAX_DRIFT_BYTES {
                "PASS"
            } else {
                "FAIL"
            }
        );
        assert!(
            drift < MAX_DRIFT_BYTES,
            "20-iteration phys_footprint drift {drift} bytes is not below the fixed {MAX_DRIFT_BYTES}-byte target"
        );
    }
}

fn published_fixture() -> TempDir {
    let directory = tempdir().expect("store directory");
    let schema = Schema::new(Vec::new()).expect("timestamp-only schema");
    let mut builder = ColumnStoreBuilder::new(schema.clone());
    for row in 0..ROWS {
        builder
            .push_row(row as i64, &[])
            .expect("fixture metadata row");
    }
    let columns = builder.finish().expect("fixture columns");
    let alive = AliveSet::new(ROWS as u32);
    let id = SegmentId::new(0x0102_0304_0506, [0x55; 10]);
    let query = rss_query_values();
    let row_bytes = DIMS.div_ceil(2);
    let mut low_codes = vec![0_u8; row_bytes];
    let mut runner_codes = vec![0_u8; row_bytes];
    let mut winner_codes = vec![0_u8; row_bytes];
    let low = query.iter().map(|value| value * -2.0).collect::<Vec<_>>();
    let runner = query.clone();
    let winner = query.iter().map(|value| value * 4.0).collect::<Vec<_>>();
    let low_factors = quantize_bit4(&low, &mut low_codes).expect("low fixture row");
    let runner_factors = quantize_bit4(&runner, &mut runner_codes).expect("runner fixture row");
    let winner_factors = quantize_bit4(&winner, &mut winner_codes).expect("winner fixture row");
    let mut codes = vec![0_u8; ROWS * row_bytes];
    let mut factors = Vec::with_capacity(ROWS);
    for row in 0..ROWS {
        let (encoded, factor) = match row {
            WINNER_ROW => (&winner_codes, winner_factors),
            RUNNER_UP_ROW => (&runner_codes, runner_factors),
            _ => (&low_codes, low_factors),
        };
        let start = row * row_bytes;
        codes[start..start + row_bytes].copy_from_slice(encoded);
        factors.push(factor);
    }
    let rescore = vec![0.0_f32; ROWS * DIMS];
    let policy = DurabilityPolicy::new(DurabilityMode::Derived, CommitTier::Ordered)
        .expect("derived policy");
    let segment = write_segment(
        &StdVfs,
        directory.path(),
        SegmentBuild {
            id,
            scheme: 4,
            dims: DIMS as u32,
            codes: &codes,
            factors: SegmentFactors::Bit4(&factors),
            rescore: &rescore,
            columns: &columns,
            alive: &alive,
        },
        policy,
    )
    .expect("fixture segment");
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
        policy,
    )
    .expect("fixture manifest");
    directory
}

fn rss_query_values() -> Vec<f32> {
    (0..DIMS)
        .map(|column| match column % 4 {
            0 => -1.0,
            1 => -0.25,
            2 => 0.5,
            _ => 1.5,
        })
        .collect()
}

#[cfg(target_os = "macos")]
fn read_load1() -> Option<f64> {
    let mut load1 = 0.0_f64;
    let read = unsafe {
        // SAFETY: `load1` is writable storage for the one requested sample.
        libc::getloadavg(&raw mut load1, 1)
    };
    (read == 1).then_some(load1)
}

#[cfg(target_os = "macos")]
fn process_is_sandboxed() -> bool {
    const SANDBOX_FILTER_NONE: libc::c_int = 0;
    unsafe extern "C" {
        fn sandbox_check(
            pid: libc::pid_t,
            operation: *const libc::c_char,
            filter_type: libc::c_int,
            ...
        ) -> libc::c_int;
    }

    unsafe {
        // SAFETY: this is the documented current-process sandbox query with
        // no operation string and therefore no variadic filter arguments.
        sandbox_check(libc::getpid(), std::ptr::null(), SANDBOX_FILTER_NONE) > 0
    }
}
