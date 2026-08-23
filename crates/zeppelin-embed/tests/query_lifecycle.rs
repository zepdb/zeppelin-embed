#![allow(clippy::expect_used)]

#[allow(dead_code)]
mod lifecycle_support;

use std::sync::Arc;
use std::time::{Duration, Instant};

use lifecycle_support::test_guard;
use tempfile::tempdir;
use zeppelin_embed::lifecycle::durability::DurabilityPolicyError;
use zeppelin_embed::lifecycle::lock::StoreLockError;
use zeppelin_embed::lifecycle::{
    CancelToken, Deadline, DeadlineError, OpenOptions, QueryControl, QueryError, Store, StoreError,
};
use zeppelin_embed::manifest::ManifestError;
use zeppelin_embed::quant::{Bit4Factors, prepare_bit4_query, prepare_int8_query};
use zeppelin_embed::scan::{
    F32Rows, Int8Factors, ScanError, ScanOptions, ScanQuery, ScanRequest, ScanRows,
};
use zeppelin_embed::segment::SegmentError;
use zeppelin_embed::wal::WalReadError;

#[test]
fn slow_scan_with_deadline_returns_typed_timeout() {
    const ROWS: usize = 50_000_000;

    let _guard = test_guard();
    let directory = tempdir().expect("store directory");
    let store = Store::open(directory.path(), OpenOptions::default()).expect("open store");
    let query = [1.0_f32];
    let rows = F32Rows::new(vec![1.0_f32; ROWS]);
    let before = store.stats().expect("stats before timed query");
    let deadline = Deadline::after(Duration::from_millis(5)).expect("representable deadline");

    let started = Instant::now();
    let result = store.top_k_with_options(
        ScanRequest {
            query: ScanQuery::F32(&query),
            rows: ScanRows::F32RowMajor(&rows),
            row_mask: None,
        },
        10,
        ScanOptions { thread_budget: 1 },
        QueryControl::Deadline(deadline),
    );
    let elapsed = started.elapsed();

    assert!(
        matches!(result, Err(QueryError::Timeout { partial: false })),
        "deadline returned {result:?}"
    );
    assert!(
        elapsed < Duration::from_millis(50),
        "5 ms deadline returned after {elapsed:?}"
    );
    let after = store.stats().expect("stats after timed query");
    assert!(after.query_pool_bytes > 0, "worker arena was not accounted");
    assert_eq!(
        after.resident_owned_bytes,
        before
            .resident_owned_bytes
            .checked_add(after.query_pool_bytes)
            .expect("pool accounting sum")
    );
    assert_eq!(after.mapped_bytes, before.mapped_bytes);
    assert_eq!(after.open_files, before.open_files);
    store.close().expect("close after timed query");
}

#[test]
fn cancelled_query_releases_its_snapshot_lease() {
    let _guard = test_guard();
    let directory = tempdir().expect("store directory");
    let store = Store::open(directory.path(), OpenOptions::default()).expect("open store");
    let query = [1.0_f32];
    let rows = F32Rows::new(vec![1.0_f32; 4_096]);
    let token = CancelToken::new();
    token.cancel();

    let result = store.top_k_with_options(
        ScanRequest {
            query: ScanQuery::F32(&query),
            rows: ScanRows::F32RowMajor(&rows),
            row_mask: None,
        },
        10,
        ScanOptions { thread_budget: 1 },
        QueryControl::Cancel(token),
    );

    assert!(matches!(
        result,
        Err(QueryError::Cancelled { partial: false })
    ));
    let stats = store.stats().expect("stats after cancellation");
    assert_eq!(stats.active_queries, 0);
    assert_eq!(
        stats.active_snapshot_leases, 0,
        "cancelled query retained its snapshot lease"
    );
    store.close().expect("cancelled lease lets close finish");
}

#[test]
fn in_flight_cancel_token_interrupts_without_partial_results() {
    const ROWS: usize = 10_000_000;

    let _guard = test_guard();
    let directory = tempdir().expect("store directory");
    let store =
        Arc::new(Store::open(directory.path(), OpenOptions::default()).expect("open store"));
    let rows = Arc::new(F32Rows::new(vec![1.0_f32; ROWS]));
    let token = CancelToken::default();
    let query_store = Arc::clone(&store);
    let query_rows = Arc::clone(&rows);
    let query_token = token.clone();
    let query = std::thread::spawn(move || {
        let query = [1.0_f32];
        query_store.top_k_with_options(
            ScanRequest {
                query: ScanQuery::F32(&query),
                rows: ScanRows::F32RowMajor(&query_rows),
                row_mask: None,
            },
            10,
            ScanOptions { thread_budget: 1 },
            QueryControl::Cancel(query_token),
        )
    });

    let admission_deadline = Instant::now()
        .checked_add(Duration::from_secs(5))
        .expect("representable admission deadline");
    loop {
        let stats = store.stats().expect("stats while open");
        if stats.active_queries == 1 {
            assert_eq!(
                stats.active_snapshot_leases, 1,
                "admitted query did not retain exactly one snapshot lease"
            );
            break;
        }
        assert!(
            Instant::now() < admission_deadline,
            "cancelled query was never observed as admitted"
        );
        std::thread::yield_now();
    }

    let cancellation_started = Instant::now();
    token.cancel();

    let result = query.join().expect("cancelled query returned");
    let cancellation_elapsed = cancellation_started.elapsed();
    assert!(matches!(
        result,
        Err(QueryError::Cancelled { partial: false })
    ));
    assert!(
        cancellation_elapsed < Duration::from_millis(50),
        "caller cancellation returned after {cancellation_elapsed:?}"
    );
    let stats = store.stats().expect("stats after in-flight cancellation");
    assert_eq!(stats.active_queries, 0);
    assert_eq!(stats.active_snapshot_leases, 0);
    store.close().expect("close after in-flight cancellation");
}

#[test]
fn admitted_read_observes_close_cancellation_without_caller_cooperation() {
    const ROWS: usize = 10_000_000;

    let _guard = test_guard();
    let directory = tempdir().expect("store directory");
    let options = OpenOptions::new().with_reader_drain_timeout(Duration::ZERO);
    let store = Arc::new(Store::open(directory.path(), options).expect("open store"));
    let rows = Arc::new(F32Rows::new(vec![1.0_f32; ROWS]));
    let query_store = Arc::clone(&store);
    let query_rows = Arc::clone(&rows);
    let query = std::thread::spawn(move || {
        let query = [1.0_f32];
        query_store.top_k_with_options(
            ScanRequest {
                query: ScanQuery::F32(&query),
                rows: ScanRows::F32RowMajor(&query_rows),
                row_mask: None,
            },
            10,
            ScanOptions { thread_budget: 1 },
            QueryControl::Cancel(CancelToken::new()),
        )
    });

    let admission_deadline = Instant::now()
        .checked_add(Duration::from_secs(5))
        .expect("representable admission deadline");
    loop {
        let stats = store.stats().expect("stats while open");
        if stats.active_queries == 1 {
            assert_eq!(
                stats.active_snapshot_leases, 1,
                "admitted query did not retain exactly one snapshot lease"
            );
            break;
        }
        assert!(
            Instant::now() < admission_deadline,
            "long query was never observed as admitted"
        );
        std::thread::yield_now();
    }

    let close_started = Instant::now();
    store
        .close()
        .expect("close cancels admitted production query");
    let close_elapsed = close_started.elapsed();
    let result = query.join().expect("query thread returned");
    assert!(matches!(
        result,
        Err(QueryError::ReadCancelled { partial: false })
    ));
    assert!(
        close_elapsed < Duration::from_millis(50),
        "close waited {close_elapsed:?} for a scan that did not cooperate"
    );
}

#[test]
fn persistent_query_pool_reuses_threads_across_queries() {
    let _guard = test_guard();
    let directory = tempdir().expect("store directory");
    let store = Store::open(directory.path(), OpenOptions::default()).expect("open store");
    let before = store.stats().expect("stats before starting query pool");
    let capacity = zeppelin_embed::scan::physical_thread_capacity().expect("worker capacity");
    let query = [1.0_f32];
    let rows = F32Rows::new(vec![1.0_f32; capacity.max(2) * 64]);
    let request = ScanRequest {
        query: ScanQuery::F32(&query),
        rows: ScanRows::F32RowMajor(&rows),
        row_mask: None,
    };

    let first = store
        .top_k_with_options(
            request,
            10,
            ScanOptions {
                thread_budget: capacity,
            },
            QueryControl::Deadline(
                Deadline::after(Duration::from_secs(5)).expect("first deadline"),
            ),
        )
        .expect("first pooled query");
    let second = store
        .top_k_with_options(
            request,
            10,
            ScanOptions { thread_budget: 0 },
            QueryControl::Deadline(
                Deadline::after(Duration::from_secs(5)).expect("second deadline"),
            ),
        )
        .expect("second pooled query");

    assert_eq!(first.stats.worker_thread_ids.len(), capacity);
    assert_eq!(second.stats.worker_thread_ids.len(), capacity);
    assert_eq!(
        second.stats.worker_thread_ids, first.stats.worker_thread_ids,
        "production partitions ran on different OS threads across queries"
    );
    let stats = store.stats().expect("stats with parked query pool");
    assert_eq!(stats.open_files, 1, "parked workers are not file handles");
    assert_eq!(stats.active_queries, 0);
    assert!(stats.query_pool_bytes > 0, "worker arena was not accounted");
    assert_eq!(
        stats.resident_owned_bytes,
        before
            .resident_owned_bytes
            .checked_add(stats.query_pool_bytes)
            .expect("pool accounting sum"),
        "parked worker arena was absent from resident-owned accounting"
    );
    store.close().expect("close reused pool");
}

#[test]
fn query_validation_and_error_surfaces_remain_typed() {
    use std::error::Error as _;

    let _guard = test_guard();
    let deadline_error = Deadline::after(Duration::MAX).expect_err("unrepresentable deadline");
    assert_eq!(deadline_error, DeadlineError::OutOfRange);
    assert_eq!(
        deadline_error.to_string(),
        "query deadline is outside Instant's range"
    );
    assert!(deadline_error.source().is_none());

    let timeout = QueryError::Timeout { partial: false };
    let cancelled = QueryError::Cancelled { partial: false };
    let read_cancelled = QueryError::ReadCancelled { partial: false };
    assert_eq!(
        timeout.to_string(),
        "query deadline expired (partial=false)"
    );
    assert_eq!(cancelled.to_string(), "query was cancelled (partial=false)");
    assert_eq!(
        read_cancelled.to_string(),
        "store close cancelled query (partial=false)"
    );
    assert!(timeout.source().is_none());
    assert!(cancelled.source().is_none());
    assert!(read_cancelled.source().is_none());

    let directory = tempdir().expect("store directory");
    let store = Store::open(directory.path(), OpenOptions::default()).expect("open store");
    let rows = F32Rows::new(Vec::new());
    let result = store.top_k_with_options(
        ScanRequest {
            query: ScanQuery::F32(&[]),
            rows: ScanRows::F32RowMajor(&rows),
            row_mask: None,
        },
        1,
        ScanOptions { thread_budget: 1 },
        QueryControl::Cancel(CancelToken::new()),
    );
    let error = result.expect_err("zero-dimensional query");
    assert_eq!(error.to_string(), "scan dimension must not be zero");
    assert!(matches!(error, QueryError::Scan(ScanError::ZeroDimension)));
    assert_eq!(
        error.source().map(ToString::to_string),
        Some("scan dimension must not be zero".to_owned())
    );
    assert_eq!(
        store
            .stats()
            .expect("stats after invalid query")
            .active_queries,
        0
    );
    store.close().expect("close after invalid query");

    let store_error = QueryError::Store(StoreError::Closed);
    assert_eq!(store_error.to_string(), "store is closed");
    assert_eq!(
        store_error.source().map(ToString::to_string),
        Some("store is closed".to_owned())
    );

    let path = std::path::PathBuf::from("fixture-store");
    let io = StoreError::Io {
        path: path.clone(),
        source: std::io::Error::other("open failed"),
    };
    assert_eq!(io.to_string(), "store I/O fixture-store: open failed");
    assert_eq!(
        io.source().map(ToString::to_string),
        Some("open failed".to_owned())
    );
    let busy = StoreError::StoreBusy { path };
    assert_eq!(
        busy.to_string(),
        "store already has a writer: fixture-store"
    );
    assert!(busy.source().is_none());
    assert_eq!(StoreError::Closing.to_string(), "store is closing");
    assert_eq!(
        StoreError::ReadCancelled.to_string(),
        "store close cancelled the admitted read"
    );

    let background_start = StoreError::BackgroundStart {
        source: std::io::Error::other("thread quota"),
    };
    assert_eq!(
        background_start.to_string(),
        "store lifecycle thread could not start: thread quota"
    );
    assert_eq!(
        background_start.source().map(ToString::to_string),
        Some("thread quota".to_owned())
    );
    assert_eq!(
        StoreError::BackgroundHandshake.to_string(),
        "store lifecycle thread startup handshake failed"
    );
    assert_eq!(
        StoreError::BackgroundThreadPanicked.to_string(),
        "store lifecycle thread panicked"
    );

    let pool_start = StoreError::QueryPoolStart {
        source: std::io::Error::other("thread quota"),
    };
    assert_eq!(
        pool_start.to_string(),
        "store query worker could not start: thread quota"
    );
    assert_eq!(
        pool_start.source().map(ToString::to_string),
        Some("thread quota".to_owned())
    );
    assert_eq!(
        StoreError::QueryPoolHandshake.to_string(),
        "store query worker startup handshake failed"
    );
    assert_eq!(
        StoreError::QueryPoolThreadPanicked.to_string(),
        "store query worker panicked"
    );
    assert_eq!(
        StoreError::QueryPoolCapacity {
            source: "topology unavailable".to_owned(),
        }
        .to_string(),
        "store query worker capacity failed: topology unavailable"
    );
    assert_eq!(
        StoreError::Synchronization {
            component: "query pool",
        }
        .to_string(),
        "store lifecycle synchronization poisoned: query pool"
    );

    let lock = StoreError::Lock(StoreLockError::Io {
        path: "fixture.lock".into(),
        source: std::io::Error::other("lock failed"),
    });
    assert_eq!(lock.to_string(), "store lock fixture.lock: lock failed");
    assert_eq!(
        lock.source().map(ToString::to_string),
        Some("store lock fixture.lock: lock failed".to_owned())
    );
    let durability = StoreError::Durability(DurabilityPolicyError::AttachedNotYetSupported);
    assert_eq!(
        durability.to_string(),
        "attached durability mode is not yet supported"
    );
    assert_eq!(
        durability.source().map(ToString::to_string),
        Some("attached durability mode is not yet supported".to_owned())
    );
    let manifest = StoreError::Manifest(ManifestError::Decode("bad payload".to_owned()));
    assert_eq!(manifest.to_string(), "manifest decode failed: bad payload");
    assert_eq!(
        manifest.source().map(ToString::to_string),
        Some("manifest decode failed: bad payload".to_owned())
    );
    let segment = StoreError::Segment(SegmentError::Geometry("bad rows".to_owned()));
    assert_eq!(segment.to_string(), "segment geometry is invalid: bad rows");
    assert_eq!(
        segment.source().map(ToString::to_string),
        Some("segment geometry is invalid: bad rows".to_owned())
    );
    let wal = StoreError::Wal(WalReadError::Io(std::io::Error::other("short read")));
    assert_eq!(wal.to_string(), "WAL read: short read");
    assert_eq!(
        wal.source().map(ToString::to_string),
        Some("WAL read: short read".to_owned())
    );
}

#[test]
fn deadline_cancellation_is_checked_inside_every_encoded_scan_loop() {
    const DIMENSIONS: usize = 512;
    const ROWS: usize = 100_000;

    let _guard = test_guard();
    let directory = tempdir().expect("store directory");
    let store = Store::open(directory.path(), OpenOptions::default()).expect("open store");

    let f16_query = vec![0x3c00_u16; DIMENSIONS];
    let f16_rows = vec![0x3c00_u16; ROWS * DIMENSIONS];
    assert_deadline_interrupts(
        &store,
        ScanRequest {
            query: ScanQuery::F16(&f16_query),
            rows: ScanRows::F16RowMajor(&f16_rows),
            row_mask: None,
        },
        "F16",
    );
    drop(f16_rows);

    let query_values = vec![1.0_f32; DIMENSIONS];
    let int8_query = prepare_int8_query(&query_values).expect("Int8 query");
    let int8_codes = vec![1_i8; ROWS * DIMENSIONS];
    let int8_factors = vec![Int8Factors::new(1.0, 0.0).expect("Int8 factors"); ROWS];
    assert_deadline_interrupts(
        &store,
        ScanRequest {
            query: ScanQuery::Int8(&int8_query),
            rows: ScanRows::Int8RowMajor {
                codes: &int8_codes,
                factors: &int8_factors,
            },
            row_mask: None,
        },
        "Int8",
    );
    drop((int8_codes, int8_factors));

    let bit4_query = prepare_bit4_query(&query_values, 17).expect("Bit4 query");
    let bit4_codes = vec![0x88_u8; ROWS * DIMENSIONS.div_ceil(2)];
    let bit4_factors = vec![Bit4Factors::from_persisted(1.0, 1.0, 1.0); ROWS];
    assert_deadline_interrupts(
        &store,
        ScanRequest {
            query: ScanQuery::Bit4(&bit4_query),
            rows: ScanRows::Bit4RowMajor {
                codes: &bit4_codes,
                factors: &bit4_factors,
            },
            row_mask: None,
        },
        "Bit4",
    );
    store.close().expect("close after encoded deadlines");
}

fn assert_deadline_interrupts(store: &Store, request: ScanRequest<'_>, scheme: &str) {
    let deadline = Deadline::after(Duration::from_millis(5)).expect("deadline");
    let started = Instant::now();
    let result = store.top_k_with_options(
        request,
        10,
        ScanOptions { thread_budget: 1 },
        QueryControl::Deadline(deadline),
    );
    let elapsed = started.elapsed();
    assert!(
        matches!(result, Err(QueryError::Timeout { partial: false })),
        "{scheme} deadline returned {result:?}"
    );
    assert!(
        elapsed < Duration::from_millis(50),
        "{scheme} loop ignored cancellation for {elapsed:?}"
    );
}
