//! Task 22 phase 3 soak: open -> ingest -> query -> close through the C
//! ABI, ZE_FFI_SOAK_ITERATIONS times (default 10,000), asserting the
//! process physical footprint reported by `ze_stats` stays flat. Ignored by
//! default because it takes minutes; CI runs it explicitly on macOS where
//! `phys_footprint` exists. Under a sanitizer build it doubles as the
//! boundary-level memory-error sweep.

mod common;

use std::mem::size_of;

use zeppelin_embed_ffi::*;

const DIMENSION: usize = 16;
const ROWS: usize = 32;
const MAX_DRIFT_BYTES: u64 = 8 * 1024 * 1024;

fn iterations() -> usize {
    std::env::var("ZE_FFI_SOAK_ITERATIONS")
        .ok()
        .and_then(|value| value.parse().ok())
        .unwrap_or(10_000)
}

fn footprint(handle: ZeHandle) -> Option<u64> {
    let mut stats: ZeStatsReport = common::sized_zeroed();
    assert_eq!(ze_stats(handle, &mut stats), ZeErrorCode::ZeOk);
    (stats.has_phys_footprint == 1).then_some(stats.phys_footprint)
}

fn one_round(path: &std::path::Path, probe: &[f32]) -> Option<u64> {
    let (code, handle) = common::open_path(path);
    assert_eq!(code, ZeErrorCode::ZeOk, "open");
    assert_eq!(
        common::ingest_rows(handle, ROWS, DIMENSION),
        ZeErrorCode::ZeOk,
        "ingest"
    );
    let mut request = common::valid_query_request(probe);
    request.k = 8;
    let mut result: ZeQueryResult = common::sized_zeroed();
    assert_eq!(ze_query(handle, &request, &mut result), ZeErrorCode::ZeOk);
    assert_eq!(result.hit_count, 8);
    assert_eq!(ze_query_result_free(&mut result), ZeErrorCode::ZeOk);
    let mut search: ZeSearchResult = common::sized_zeroed();
    let mut search_request = common::valid_search_request(probe);
    search_request.k = 8;
    assert_eq!(
        ze_search(handle, &search_request, &mut search),
        ZeErrorCode::ZeOk
    );
    assert_eq!(ze_search_result_free(&mut search), ZeErrorCode::ZeOk);
    let seal = ZeSealRequest {
        abi_size: size_of::<ZeSealRequest>() as u32,
        abi_reserved: 0,
        cancel_token: 0,
    };
    let mut generation: ZeGenerationReport = common::sized_zeroed();
    assert_eq!(ze_seal(handle, &seal, &mut generation), ZeErrorCode::ZeOk);
    let measured = footprint(handle);
    assert_eq!(ze_close(handle), ZeErrorCode::ZeOk, "close");
    measured
}

#[test]
#[ignore = "minutes-long soak; run explicitly with ZE_FFI_SOAK_ITERATIONS"]
fn ten_thousand_open_ingest_query_close_rounds_keep_the_footprint_flat() {
    let directory = tempfile::tempdir().expect("temporary directory");
    let probe = (0..DIMENSION)
        .map(|index| index as f32 * 0.5)
        .collect::<Vec<_>>();
    let iterations = iterations();
    let warmup = iterations.min(100);
    let mut baseline = None;
    let mut peak = 0_u64;
    let mut last = None;
    for round in 0..iterations {
        let path = directory.path().join(format!("store-{}", round % 8));
        let _ = std::fs::remove_dir_all(&path);
        let measured = one_round(&path, &probe);
        if let Some(bytes) = measured {
            peak = peak.max(bytes);
            last = Some(bytes);
            if round == warmup {
                baseline = Some(bytes);
            }
        }
    }
    match (baseline, last) {
        (Some(baseline), Some(last)) => {
            let drift = last.abs_diff(baseline);
            eprintln!(
                "FFI_SOAK iterations={iterations} baseline_bytes={baseline} \
                 final_bytes={last} peak_bytes={peak} drift_bytes={drift}"
            );
            assert!(
                drift < MAX_DRIFT_BYTES,
                "footprint drifted by {drift} bytes over {iterations} rounds"
            );
        }
        _ => eprintln!("FFI_SOAK iterations={iterations} NOT MEASURED: phys_footprint unavailable"),
    }
}
