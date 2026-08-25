//! R13 end-to-end baseline at the public `Store::search` seam.

use std::alloc::{GlobalAlloc, Layout, System};
use std::error::Error;
use std::hint::black_box;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};

use tempfile::tempdir;
use zeppelin_embed::ingest::{
    DocId, DocumentVersion, IngestBatch, IngestDocument, Revision, SearchRequest,
};
use zeppelin_embed::lifecycle::{
    CancelToken, OpenOptions, QueryControl, SearchOptions, SearchTier, Store,
};
use zeppelin_embed::scan::ScanOptions;
use zeppelin_embed_bench::platform::memory_graph::verify_bench_profile;
use zeppelin_embed_bench::platform::taint::{
    detect_taint, format_load1, format_taint_labels, print_taint_status,
};

const ROWS: usize = 10_000;
const DIMENSIONS: usize = 128;
const TOP_K: usize = 10;
const WARMUPS: usize = 3;
const QUERIES: usize = 31;
const SEED: u64 = 0x13_2026_0825;
const LOAD_LIMIT: f64 = 1.0;

static TRACK_ALLOCATIONS: AtomicBool = AtomicBool::new(false);
static ALLOCATION_CALLS: AtomicU64 = AtomicU64::new(0);
static ALLOCATION_BYTES: AtomicU64 = AtomicU64::new(0);

struct CountingAllocator;

#[global_allocator]
static GLOBAL_ALLOCATOR: CountingAllocator = CountingAllocator;

unsafe impl GlobalAlloc for CountingAllocator {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        let pointer = unsafe { System.alloc(layout) };
        if !pointer.is_null() {
            record_allocation(layout.size());
        }
        pointer
    }

    unsafe fn alloc_zeroed(&self, layout: Layout) -> *mut u8 {
        let pointer = unsafe { System.alloc_zeroed(layout) };
        if !pointer.is_null() {
            record_allocation(layout.size());
        }
        pointer
    }

    unsafe fn dealloc(&self, pointer: *mut u8, layout: Layout) {
        unsafe { System.dealloc(pointer, layout) };
    }

    unsafe fn realloc(&self, pointer: *mut u8, layout: Layout, new_size: usize) -> *mut u8 {
        let new_pointer = unsafe { System.realloc(pointer, layout, new_size) };
        if !new_pointer.is_null() {
            record_allocation(new_size);
        }
        new_pointer
    }
}

fn record_allocation(bytes: usize) {
    if TRACK_ALLOCATIONS.load(Ordering::Relaxed) {
        ALLOCATION_CALLS.fetch_add(1, Ordering::Relaxed);
        ALLOCATION_BYTES.fetch_add(bytes as u64, Ordering::Relaxed);
    }
}

fn main() {
    if let Err(error) = run() {
        eprintln!("store-seam-baseline: {error}");
        std::process::exit(1);
    }
}

fn run() -> Result<(), Box<dyn Error>> {
    verify_bench_profile()?;
    let taint = detect_taint(LOAD_LIMIT);
    println!(
        "STORE_SEAM_CONTEXT machine=Apple_M3_Max model=Mac15,9 profile=bench opt_level={} dataset=synthetic rows={ROWS} dimensions={DIMENSIONS} k={TOP_K} queries={QUERIES} warmups={WARMUPS} seed=0x{SEED:016x} tier=scan thread_budget=1 checksum_validation_bytes=UNAVAILABLE allocation_scope=whole_process_query_window",
        env!("ZEPPELIN_BENCH_OPT_LEVEL")
    );
    print_taint_status(&taint, LOAD_LIMIT, "Store-seam baseline");

    let directory = tempdir()?;
    let store = Store::open(directory.path(), OpenOptions::default())?;
    store.ingest(IngestBatch::new(fixture_documents()))?;
    store.seal()?;
    let options = SearchOptions::new(ScanOptions { thread_budget: 1 }).with_tier(SearchTier::Scan);
    for warmup in 0..WARMUPS {
        let query = query_vector(warmup);
        black_box(store.search(
            SearchRequest::new(&query),
            TOP_K,
            options,
            QueryControl::Cancel(CancelToken::new()),
        )?);
    }

    let mut elapsed_us = Vec::with_capacity(QUERIES);
    let mut allocation_calls = Vec::with_capacity(QUERIES);
    let mut allocation_bytes = Vec::with_capacity(QUERIES);
    let mut scan_bytes = Vec::with_capacity(QUERIES);
    let mut scan_dimensions = Vec::with_capacity(QUERIES);
    let mut checksums = Vec::with_capacity(QUERIES);
    let mut first_plan = None;
    for query_index in 0..QUERIES {
        let query = query_vector(query_index + WARMUPS);
        begin_allocation_window();
        let outcome = store.search(
            SearchRequest::new(&query),
            TOP_K,
            options,
            QueryControl::Cancel(CancelToken::new()),
        );
        let allocation = end_allocation_window();
        let outcome = outcome?;
        let checksum = candidate_checksum(&outcome.candidates);
        let diagnostics = &outcome.diagnostics;
        if first_plan.is_none() {
            first_plan = Some(format!("{:?}", diagnostics.plan));
        }
        elapsed_us.push(diagnostics.elapsed.as_secs_f64() * 1e6);
        allocation_calls.push(allocation.calls);
        allocation_bytes.push(allocation.bytes);
        scan_bytes.push(diagnostics.counters.scan.bytes_read);
        scan_dimensions.push(diagnostics.counters.scan.dims_touched);
        checksums.push(checksum);
        println!(
            "STORE_SEAM_QUERY query={query_index} elapsed_us={:.6} result_checksum={checksum} result_checksum_input_bytes={} allocation_calls={} allocation_bytes={} scan_bytes_read={} scan_dims_touched={} scan_threads={} graph_segments={} graph_candidates_scored={} graph_candidates_rescored={} returned={} approximate={} exact_rescore={} qos_class={:?} qos_priority={} load1={} taint={}",
            diagnostics.elapsed.as_secs_f64() * 1e6,
            outcome.candidates.len() * (std::mem::size_of::<u128>() + std::mem::size_of::<f32>()),
            allocation.calls,
            allocation.bytes,
            diagnostics.counters.scan.bytes_read,
            diagnostics.counters.scan.dims_touched,
            diagnostics.counters.scan.threads_used,
            diagnostics.counters.graph.segments_traversed,
            diagnostics.counters.graph.candidates_scored,
            diagnostics.counters.graph.candidates_rescored,
            diagnostics.returned,
            diagnostics.approximate,
            diagnostics.exact_rescore,
            diagnostics.observed_qos.class,
            diagnostics.observed_qos.relative_priority,
            format_load1(taint.load1),
            format_taint_labels(&taint.taints),
        );
    }
    elapsed_us.sort_by(f64::total_cmp);
    allocation_calls.sort_unstable();
    allocation_bytes.sort_unstable();
    scan_bytes.sort_unstable();
    scan_dimensions.sort_unstable();
    checksums.sort_unstable();
    println!(
        "STORE_SEAM_RESULT median_elapsed_us={:.6} median_allocation_calls={} median_allocation_bytes={} median_scan_bytes_read={} median_scan_dims_touched={} checksum_min={} checksum_max={} plan={} checksum_validation_bytes=UNAVAILABLE checksum_gap=core_QueryDiagnostics_has_no_checksum_validation_byte_counter allocation_authority=bench_global_allocator_not_core_attribution load1={} taint={}",
        median_f64(&elapsed_us),
        median_u64(&allocation_calls),
        median_u64(&allocation_bytes),
        median_u64(&scan_bytes),
        median_u64(&scan_dimensions),
        checksums.first().copied().unwrap_or(0),
        checksums.last().copied().unwrap_or(0),
        first_plan
            .unwrap_or_else(|| String::from("[]"))
            .replace(' ', "_"),
        format_load1(taint.load1),
        format_taint_labels(&taint.taints),
    );
    store.close()?;
    Ok(())
}

#[derive(Clone, Copy)]
struct AllocationObservation {
    calls: u64,
    bytes: u64,
}

fn begin_allocation_window() {
    ALLOCATION_CALLS.store(0, Ordering::Relaxed);
    ALLOCATION_BYTES.store(0, Ordering::Relaxed);
    TRACK_ALLOCATIONS.store(true, Ordering::Release);
}

fn end_allocation_window() -> AllocationObservation {
    TRACK_ALLOCATIONS.store(false, Ordering::Release);
    AllocationObservation {
        calls: ALLOCATION_CALLS.load(Ordering::Relaxed),
        bytes: ALLOCATION_BYTES.load(Ordering::Relaxed),
    }
}

fn fixture_documents() -> Vec<IngestDocument> {
    (0..ROWS)
        .map(|row| {
            IngestDocument::new(
                DocumentVersion::new(DocId::new(row as u128 + 1), Revision::new(1)),
                generated_vector(row as u64),
            )
        })
        .collect()
}

fn query_vector(query: usize) -> Vec<f32> {
    generated_vector((query as u64).wrapping_add(SEED))
}

fn generated_vector(seed: u64) -> Vec<f32> {
    let mut state = seed ^ 0x9e37_79b9_7f4a_7c15;
    (0..DIMENSIONS)
        .map(|_| {
            state ^= state << 13;
            state ^= state >> 7;
            state ^= state << 17;
            let signed = (state as u32) as i32;
            signed as f32 / i32::MAX as f32
        })
        .collect()
}

fn candidate_checksum(candidates: &[zeppelin_embed::ingest::SearchCandidate]) -> u64 {
    candidates
        .iter()
        .fold(0xcbf2_9ce4_8422_2325, |hash, candidate| {
            let document = candidate
                .document()
                .map_or(0, |version| version.doc_id().get());
            hash.rotate_left(9)
                ^ document as u64
                ^ (document >> 64) as u64
                ^ u64::from(candidate.score().to_bits())
        })
}

fn median_f64(values: &[f64]) -> f64 {
    values[values.len() / 2]
}

fn median_u64(values: &[u64]) -> u64 {
    values[values.len() / 2]
}
