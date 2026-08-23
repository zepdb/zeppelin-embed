use std::error::Error;
use std::time::Instant;

use tempfile::tempdir;
use zeppelin_embed::lifecycle::{CancelToken, OpenOptions, QueryControl, Store};
use zeppelin_embed::scan::{F32Rows, ScanOptions, ScanQuery, ScanRequest, ScanRows};

const PARTITIONS: usize = 12;
const WARMUPS: usize = 50;
const ITERATIONS: usize = 2_000;

fn main() -> Result<(), Box<dyn Error>> {
    let capacity = zeppelin_embed::scan::physical_thread_capacity()?;
    if capacity < PARTITIONS {
        return Err(format!(
            "query-pool microbench needs {PARTITIONS} workers, machine exposes {capacity}"
        )
        .into());
    }
    let directory = tempdir()?;
    let store = Store::open(directory.path(), OpenOptions::default())?;
    let query = [1.0_f32];
    let rows = F32Rows::new(vec![1.0_f32; PARTITIONS]);
    let request = ScanRequest {
        query: ScanQuery::F32(&query),
        rows: ScanRows::F32RowMajor(&rows),
        row_mask: None,
    };
    let options = ScanOptions {
        thread_budget: PARTITIONS,
    };
    let token = CancelToken::new();

    let initial =
        store.top_k_with_options(request, 0, options, QueryControl::Cancel(token.clone()))?;
    if initial.stats.threads_used != PARTITIONS
        || initial.stats.worker_thread_ids.len() != PARTITIONS
    {
        return Err(format!("query-pool microbench used unexpected workers: {initial:?}").into());
    }
    for _ in 0..WARMUPS {
        std::hint::black_box(store.top_k_with_options(
            request,
            0,
            options,
            QueryControl::Cancel(token.clone()),
        )?);
    }

    let started = Instant::now();
    for _ in 0..ITERATIONS {
        std::hint::black_box(store.top_k_with_options(
            request,
            0,
            options,
            QueryControl::Cancel(token.clone()),
        )?);
    }
    let elapsed = started.elapsed();
    let per_query_ns = elapsed.as_nanos() as f64 / ITERATIONS as f64;
    println!(
        "partitions={PARTITIONS} warmups={WARMUPS} iterations={ITERATIONS} total_ns={} per_query_ns={per_query_ns:.1}",
        elapsed.as_nanos()
    );
    store.close()?;
    Ok(())
}
