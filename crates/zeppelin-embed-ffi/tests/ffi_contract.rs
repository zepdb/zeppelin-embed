mod common;

use std::mem::{align_of, offset_of, size_of};
use std::process::Command;

use zeppelin_embed_ffi::*;

fn process_sensitive_lock() -> std::sync::MutexGuard<'static, ()> {
    static LOCK: std::sync::OnceLock<std::sync::Mutex<()>> = std::sync::OnceLock::new();
    LOCK.get_or_init(|| std::sync::Mutex::new(()))
        .lock()
        .expect("process-sensitive test mutex")
}

#[test]
fn error_codes_are_append_only() {
    let codes = [
        (ZeErrorCode::Ok, 0),
        (ZeErrorCode::InvalidArgument, 1),
        (ZeErrorCode::InvalidHandle, 2),
        (ZeErrorCode::Closed, 3),
        (ZeErrorCode::Closing, 4),
        (ZeErrorCode::Poisoned, 5),
        (ZeErrorCode::Panic, 6),
        (ZeErrorCode::Busy, 7),
        (ZeErrorCode::StoreBusy, 8),
        (ZeErrorCode::Io, 9),
        (ZeErrorCode::Corrupt, 10),
        (ZeErrorCode::Unsupported, 11),
        (ZeErrorCode::Cancelled, 12),
        (ZeErrorCode::Timeout, 13),
        (ZeErrorCode::OutOfMemory, 14),
        (ZeErrorCode::BudgetExceeded, 15),
        (ZeErrorCode::EmptyBatch, 16),
        (ZeErrorCode::StaleRevision, 17),
        (ZeErrorCode::DimensionMismatch, 18),
        (ZeErrorCode::NotFound, 19),
        (ZeErrorCode::Synchronization, 20),
        (ZeErrorCode::AccessMode, 21),
        (ZeErrorCode::Internal, 22),
        (ZeErrorCode::EpochMismatch, 23),
        (ZeErrorCode::EpochUndeclared, 24),
        (ZeErrorCode::EpochUnstamped, 25),
    ];
    for (actual, expected) in codes {
        assert_eq!(actual as i32, expected);
    }
}

macro_rules! assert_layout {
    ($type:ty, $size:literal, $align:literal, {$($field:ident: $offset:literal),+ $(,)?}) => {{
        assert_eq!(size_of::<$type>(), $size, "{} size", stringify!($type));
        assert_eq!(align_of::<$type>(), $align, "{} alignment", stringify!($type));
        $(assert_eq!(offset_of!($type, $field), $offset,
            "{}.{} offset", stringify!($type), stringify!($field));)+
    }};
}

#[test]
fn every_request_struct_has_the_frozen_size_and_field_offsets() {
    assert_layout!(ZeOpenRequest, 64, 8, {
        abi_size: 0, abi_reserved: 4, path: 8, path_len: 16,
        access_mode: 24, durability_mode: 28, commit_tier: 32,
        reader_drain_timeout_ms: 40, max_resident_bytes: 48, max_temp_bytes: 56
    });
    assert_layout!(ZeStateReport, 16, 4, {
        abi_size: 0, abi_reserved: 4, state: 8, reserved: 12
    });
    assert_layout!(ZeStatsReport, 144, 8, {
        abi_size: 0, abi_reserved: 4, resident_owned_bytes: 8, mapped_bytes: 16,
        mapped_resident_bytes: 24, segment_bytes: 32, active_segment_bytes: 40,
        active_row_count: 48, tombstone_count: 56, tombstone_bytes: 64,
        wal_bytes: 72, cache_bytes: 80, temporary_bytes: 88,
        query_pool_bytes: 96, open_files: 104, active_queries: 112,
        active_snapshot_leases: 120, phys_footprint: 128,
        has_phys_footprint: 136, reserved: 140
    });
    assert_layout!(ZeDocId, 16, 8, { high: 0, low: 8 });
    assert_layout!(ZeIngestDocument, 72, 8, {
        abi_size: 0, abi_reserved: 4, doc_id: 8, revision: 24, timestamp: 32,
        vector: 40, vector_len: 48, metadata: 56, metadata_len: 64
    });
    assert_layout!(ZeIngestRequest, 32, 8, {
        abi_size: 0, abi_reserved: 4, documents: 8, document_count: 16, dimension: 24
    });
    assert_layout!(ZeDeleteRequest, 24, 8, {
        abi_size: 0, abi_reserved: 4, doc_ids: 8, doc_id_count: 16
    });
    assert_layout!(ZeMutationReport, 24, 8, {
        abi_size: 0, abi_reserved: 4, sequence: 8, generation: 16
    });
    assert_layout!(ZeSearchRequest, 88, 8, {
        abi_size: 0, abi_reserved: 4, vector: 8, vector_len: 16,
        dimension: 24, k: 32, thread_budget: 40, search_tier: 48,
        graph_profile: 52, graph_ef: 56, graph_seed: 64,
        cancel_token: 72, deadline_ns: 80
    });
    assert_layout!(ZeSearchHit, 64, 8, {
        source_kind: 0, reserved: 4, segment_id: 8, local_row: 24,
        has_document: 28, doc_id: 32, revision: 48, score: 56, reserved_tail: 60
    });
    assert_layout!(ZeSearchResult, 112, 8, {
        abi_size: 0, abi_reserved: 4, hits: 8, hit_count: 16, generation: 24,
        dims_touched: 32, bytes_read: 40, threads_used: 48,
        graph_segments_traversed: 56, graph_validations: 64,
        graph_entry_seed_discoveries: 72, graph_visited_epoch_clears: 80,
        graph_candidates_scored: 88, graph_candidates_rescored: 96,
        graph_segments_pruned_by_bound: 104
    });
    assert_layout!(ZeSealRequest, 16, 8, {
        abi_size: 0, abi_reserved: 4, cancel_token: 8
    });
    assert_layout!(ZeGenerationReport, 16, 8, {
        abi_size: 0, abi_reserved: 4, generation: 8
    });
    assert_layout!(ZeDropPartitionRequest, 24, 8, {
        abi_size: 0, abi_reserved: 4, start_ts: 8, end_ts: 16
    });
    assert_layout!(ZeRetentionRequest, 24, 8, {
        abi_size: 0, abi_reserved: 4, window: 8, now_ts: 16
    });
    assert_layout!(ZePartitionReport, 48, 8, {
        abi_size: 0, abi_reserved: 4, generation: 8, segments_dropped: 16,
        bytes_reclaimed: 24, straddlers_skipped: 32, is_no_op: 40, reserved: 44
    });
    assert_layout!(ZePurgeRequest, 24, 8, {
        abi_size: 0, abi_reserved: 4, doc_ids: 8, doc_id_count: 16
    });
    assert_layout!(ZePurgeTokenReport, 40, 8, {
        abi_size: 0, abi_reserved: 4, token_id: 8, generation: 16,
        unknown_id_count: 24, is_no_op: 32, reserved: 36
    });
    assert_layout!(ZeAwaitPurgeRequest, 16, 8, {
        abi_size: 0, abi_reserved: 4, token_id: 8
    });
    assert_layout!(ZePurgeReport, 40, 8, {
        abi_size: 0, abi_reserved: 4, generation: 8, segments_rewritten: 16,
        unknown_id_count: 24, wal_rewritten: 32, is_no_op: 36
    });
    assert_layout!(ZeMaintainRequest, 24, 8, {
        abi_size: 0, abi_reserved: 4, wall_time_ns: 8, bytes: 16
    });
    assert_layout!(ZeMaintainReport, 40, 8, {
        abi_size: 0, abi_reserved: 4, graphs_built: 8, bytes_consumed: 16,
        checkpoints_resumed: 24, status: 32, reserved: 36
    });
}

#[test]
fn use_after_close_double_close_and_a_stale_generation_each_return_typed_errors() {
    let _process_guard = process_sensitive_lock();
    let directory = tempfile::tempdir().expect("temporary directory");
    let path = directory.path().join("store");
    let (code, first) = common::open_path(&path);
    assert_eq!(code, ZeErrorCode::Ok);
    assert_ne!(first, 0);
    assert_eq!(ze_close(first), ZeErrorCode::Ok);

    let mut state: ZeStateReport = common::sized_zeroed();
    assert_eq!(ze_state(first, &mut state), ZeErrorCode::Closed);
    assert_eq!(ze_close(first), ZeErrorCode::Closed);

    let (code, second) = common::open_path(&path);
    assert_eq!(code, ZeErrorCode::Ok);
    assert_ne!(first, second, "slot reuse must bump the generation");
    assert_eq!(ze_state(first, &mut state), ZeErrorCode::Closed);
    assert_eq!(ze_state(second, &mut state), ZeErrorCode::Ok);
    assert_eq!(ze_state(0, &mut state), ZeErrorCode::InvalidHandle);
    assert_eq!(ze_close(second), ZeErrorCode::Ok);
}

const PANIC_PROBE: u32 = 0x5041_4e49;
const POISON_TABLE_NAMES: &[&str] = &[
    "ze_apply_retention",
    "ze_await_physical_purge",
    "ze_close",
    "ze_delete",
    "ze_drop_partition",
    "ze_ingest",
    "ze_maintain",
    "ze_purge",
    "ze_seal",
    "ze_search",
    "ze_state",
    "ze_stats",
];

fn delegate_to_panic_feature(test_name: &str) -> bool {
    if cfg!(feature = "abi-panic-probe") {
        return false;
    }
    let workspace = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .and_then(std::path::Path::parent)
        .expect("workspace root");
    let status = Command::new(std::env::var("CARGO").unwrap_or_else(|_| "cargo".to_owned()))
        .current_dir(workspace)
        .args([
            "test",
            "-p",
            "zeppelin-embed-ffi",
            "--features",
            "abi-panic-probe",
            "--test",
            "ffi_contract",
            test_name,
            "--",
            "--exact",
            "--nocapture",
        ])
        .status()
        .expect("run panic-probe feature test");
    assert!(status.success(), "panic-probe feature child failed");
    true
}

fn trigger_panic(handle: ZeHandle) -> ZeErrorCode {
    let vector = [1.0_f32];
    let mut request = common::valid_search_request(&vector);
    request.abi_reserved = PANIC_PROBE;
    let mut result: ZeSearchResult = common::sized_zeroed();
    ze_search(handle, &request, &mut result)
}

#[cfg(feature = "abi-panic-probe")]
fn arm_named_panic(entry_point: &'static str) {
    zeppelin_embed_ffi::arm_abi_panic_probe(entry_point);
}

#[cfg(not(feature = "abi-panic-probe"))]
fn arm_named_panic(_entry_point: &'static str) {}

#[test]
fn a_panic_crossing_the_abi_is_caught_the_handle_is_poisoned_and_the_process_survives() {
    let _process_guard = process_sensitive_lock();
    const NAME: &str =
        "a_panic_crossing_the_abi_is_caught_the_handle_is_poisoned_and_the_process_survives";
    if delegate_to_panic_feature(NAME) {
        return;
    }
    if std::env::var_os("ZE_ABI_PANIC_CHILD").is_none() {
        let status = Command::new(std::env::current_exe().expect("current test executable"))
            .args(["--exact", NAME, "--nocapture"])
            .env("ZE_ABI_PANIC_CHILD", "1")
            .status()
            .expect("spawn ABI panic child");
        assert!(status.success(), "ABI panic child must survive");
        return;
    }

    let store = common::TestStore::new();
    assert_eq!(trigger_panic(store.handle), ZeErrorCode::Panic);
    let mut state: ZeStateReport = common::sized_zeroed();
    assert_eq!(ze_state(store.handle, &mut state), ZeErrorCode::Poisoned);

    let mut required = 0;
    assert_eq!(
        ze_last_error_message(store.handle, std::ptr::null_mut(), 0, &mut required),
        ZeErrorCode::Ok
    );
    let mut message = vec![0_i8; required + 1];
    assert_eq!(
        ze_last_error_message(
            store.handle,
            message.as_mut_ptr(),
            message.len(),
            &mut required,
        ),
        ZeErrorCode::Ok
    );
    let bytes = message
        .iter()
        .take(required)
        .map(|byte| *byte as u8)
        .collect::<Vec<_>>();
    assert_eq!(
        String::from_utf8(bytes).expect("panic UTF-8"),
        "abi panic probe"
    );
    assert_eq!(ze_close(store.handle), ZeErrorCode::Poisoned);
}

struct PoisonContext {
    store: common::TestStore,
    vector: Vec<f32>,
    ids: Vec<ZeDocId>,
}

impl PoisonContext {
    fn new() -> Self {
        Self {
            store: common::TestStore::new(),
            vector: vec![1.0],
            ids: vec![ZeDocId { high: 0, low: 1 }],
        }
    }
}

type PoisonCall = fn(&mut PoisonContext) -> ZeErrorCode;

fn poison_function_table() -> Vec<(&'static str, PoisonCall)> {
    vec![
        ("ze_state", |context| {
            let mut report: ZeStateReport = common::sized_zeroed();
            ze_state(context.store.handle, &mut report)
        }),
        ("ze_stats", |context| {
            let mut report: ZeStatsReport = common::sized_zeroed();
            ze_stats(context.store.handle, &mut report)
        }),
        ("ze_ingest", |context| {
            let document = ZeIngestDocument {
                abi_size: size_of::<ZeIngestDocument>() as u32,
                abi_reserved: 0,
                doc_id: context.ids[0],
                revision: 1,
                timestamp: 0,
                vector: context.vector.as_ptr(),
                vector_len: context.vector.len(),
                metadata: std::ptr::null(),
                metadata_len: 0,
            };
            let request = ZeIngestRequest {
                abi_size: size_of::<ZeIngestRequest>() as u32,
                abi_reserved: 0,
                documents: &document,
                document_count: 1,
                dimension: 1,
            };
            let mut report: ZeMutationReport = common::sized_zeroed();
            ze_ingest(context.store.handle, &request, &mut report)
        }),
        ("ze_delete", |context| {
            let request = ZeDeleteRequest {
                abi_size: size_of::<ZeDeleteRequest>() as u32,
                abi_reserved: 0,
                doc_ids: context.ids.as_ptr(),
                doc_id_count: context.ids.len(),
            };
            let mut report: ZeMutationReport = common::sized_zeroed();
            ze_delete(context.store.handle, &request, &mut report)
        }),
        ("ze_search", |context| {
            let request = common::valid_search_request(&context.vector);
            let mut result: ZeSearchResult = common::sized_zeroed();
            ze_search(context.store.handle, &request, &mut result)
        }),
        ("ze_seal", |context| {
            let request = ZeSealRequest {
                abi_size: size_of::<ZeSealRequest>() as u32,
                abi_reserved: 0,
                cancel_token: 0,
            };
            let mut report: ZeGenerationReport = common::sized_zeroed();
            ze_seal(context.store.handle, &request, &mut report)
        }),
        ("ze_drop_partition", |context| {
            let request = ZeDropPartitionRequest {
                abi_size: size_of::<ZeDropPartitionRequest>() as u32,
                abi_reserved: 0,
                start_ts: 0,
                end_ts: 1,
            };
            let mut report: ZePartitionReport = common::sized_zeroed();
            ze_drop_partition(context.store.handle, &request, &mut report)
        }),
        ("ze_apply_retention", |context| {
            let request = ZeRetentionRequest {
                abi_size: size_of::<ZeRetentionRequest>() as u32,
                abi_reserved: 0,
                window: 1,
                now_ts: 1,
            };
            let mut report: ZePartitionReport = common::sized_zeroed();
            ze_apply_retention(context.store.handle, &request, &mut report)
        }),
        ("ze_purge", |context| {
            let request = ZePurgeRequest {
                abi_size: size_of::<ZePurgeRequest>() as u32,
                abi_reserved: 0,
                doc_ids: context.ids.as_ptr(),
                doc_id_count: context.ids.len(),
            };
            let mut report: ZePurgeTokenReport = common::sized_zeroed();
            ze_purge(context.store.handle, &request, &mut report)
        }),
        ("ze_await_physical_purge", |context| {
            let request = ZeAwaitPurgeRequest {
                abi_size: size_of::<ZeAwaitPurgeRequest>() as u32,
                abi_reserved: 0,
                token_id: 1,
            };
            let mut report: ZePurgeReport = common::sized_zeroed();
            ze_await_physical_purge(context.store.handle, &request, &mut report)
        }),
        ("ze_maintain", |context| {
            let request = ZeMaintainRequest {
                abi_size: size_of::<ZeMaintainRequest>() as u32,
                abi_reserved: 0,
                wall_time_ns: 1,
                bytes: 1,
            };
            let mut report: ZeMaintainReport = common::sized_zeroed();
            ze_maintain(context.store.handle, &request, &mut report)
        }),
        ("ze_close", |context| ze_close(context.store.handle)),
    ]
}

#[test]
fn every_entry_point_returns_ze_err_poisoned_after_a_caught_panic() {
    let _process_guard = process_sensitive_lock();
    const NAME: &str = "every_entry_point_returns_ze_err_poisoned_after_a_caught_panic";
    if delegate_to_panic_feature(NAME) {
        return;
    }
    let table = poison_function_table();
    assert_eq!(table.len(), POISON_TABLE_NAMES.len());
    for (name, call) in table {
        let mut context = PoisonContext::new();
        arm_named_panic(name);
        assert_eq!(
            call(&mut context),
            ZeErrorCode::Panic,
            "{name} catch wrapper"
        );
        assert_eq!(call(&mut context), ZeErrorCode::Poisoned, "{name}");
    }
}

#[test]
fn the_poison_table_covers_every_exported_handle_taking_symbol() {
    let header = std::fs::read_to_string(
        std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("include/zeppelin_embed.h"),
    )
    .expect("committed C header");
    let mut declarations = String::new();
    let mut remainder = header.as_str();
    while let Some(start) = remainder.find("/*") {
        declarations.push_str(&remainder[..start]);
        let after_start = &remainder[start + 2..];
        let end = after_start.find("*/").expect("terminated header comment");
        remainder = &after_start[end + 2..];
    }
    declarations.push_str(remainder);
    let mut exported = std::collections::BTreeSet::new();
    for declaration in declarations.split(';') {
        if !declaration.contains("ze_handle handle") {
            continue;
        }
        let Some(open) = declaration.find('(') else {
            continue;
        };
        let name = declaration[..open]
            .split_whitespace()
            .last()
            .expect("function name")
            .to_owned();
        if name != "ze_last_error_message" {
            exported.insert(name);
        }
    }
    let table = poison_function_table()
        .into_iter()
        .map(|(name, _)| name.to_owned())
        .collect::<std::collections::BTreeSet<_>>();
    assert_eq!(exported, table);
    assert_eq!(
        table,
        POISON_TABLE_NAMES
            .iter()
            .map(|name| (*name).to_owned())
            .collect()
    );
}

#[test]
fn a_second_concurrent_writer_call_returns_ze_err_busy() {
    let store = common::TestStore::new();
    let handle = store.handle;
    let (started_tx, started_rx) = std::sync::mpsc::channel();
    let writer = std::thread::spawn(move || {
        started_tx.send(()).expect("signal writer start");
        common::ingest_rows(handle, 500, 256)
    });
    started_rx.recv().expect("writer start");
    std::thread::sleep(std::time::Duration::from_millis(1));

    let request = ZeMaintainRequest {
        abi_size: size_of::<ZeMaintainRequest>() as u32,
        abi_reserved: 0,
        wall_time_ns: 1,
        bytes: 1,
    };
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
    let observed = loop {
        let mut report: ZeMaintainReport = common::sized_zeroed();
        let code = ze_maintain(handle, &request, &mut report);
        if code == ZeErrorCode::Busy {
            break code;
        }
        assert_eq!(
            code,
            ZeErrorCode::Ok,
            "second writer returned a typed result"
        );
        assert!(
            std::time::Instant::now() < deadline,
            "second writer never observed ZE_ERR_BUSY before the hard deadline"
        );
        std::thread::yield_now();
    };
    assert_eq!(observed, ZeErrorCode::Busy);
    assert_eq!(writer.join().expect("writer thread"), ZeErrorCode::Ok);
}

#[test]
fn a_second_process_opening_the_same_path_gets_store_busy() {
    let _process_guard = process_sensitive_lock();
    const NAME: &str = "a_second_process_opening_the_same_path_gets_store_busy";
    if let Some(path) = std::env::var_os("ZE_STORE_BUSY_CHILD_PATH") {
        let (code, handle) = common::open_path(std::path::Path::new(&path));
        assert_eq!(code, ZeErrorCode::StoreBusy);
        assert_eq!(handle, 0);
        return;
    }
    let store = common::TestStore::new();
    let status = Command::new(std::env::current_exe().expect("current test executable"))
        .args(["--exact", NAME, "--nocapture"])
        .env("ZE_STORE_BUSY_CHILD_PATH", &store.path)
        .status()
        .expect("spawn second-process open");
    assert!(status.success(), "second-process open child failed");
}

#[test]
fn cancellation_mid_query_returns_typed_cancelled_with_no_candidates_through_the_boundary() {
    let store = common::TestStore::new();
    assert_eq!(
        common::ingest_rows(store.handle, 100, 8_192),
        ZeErrorCode::Ok
    );
    let mut token = 0;
    assert_eq!(ze_cancel_token_create(&mut token), ZeErrorCode::Ok);
    let handle = store.handle;
    let (started_tx, started_rx) = std::sync::mpsc::channel();
    let query = std::thread::spawn(move || {
        let vector = vec![0.25_f32; 8_192];
        let mut request = common::valid_search_request(&vector);
        request.k = 100;
        request.thread_budget = 1;
        request.cancel_token = token;
        let mut result: ZeSearchResult = common::sized_zeroed();
        started_tx.send(()).expect("signal query start");
        let code = ze_search(handle, &request, &mut result);
        let observation = (code, result.hit_count, result.hits.is_null());
        if code == ZeErrorCode::Ok {
            assert_eq!(ze_search_result_free(&mut result), ZeErrorCode::Ok);
        }
        observation
    });
    started_rx.recv().expect("query start");
    std::thread::sleep(std::time::Duration::from_millis(1));
    assert_eq!(ze_cancel_token_cancel(token), ZeErrorCode::Ok);
    let (code, hit_count, hits_are_null) = query.join().expect("query thread");
    assert_eq!(code, ZeErrorCode::Cancelled);
    assert_eq!(hit_count, 0);
    assert!(hits_are_null);
    assert_eq!(ze_cancel_token_free(token), ZeErrorCode::Ok);
}

#[test]
fn a_deadline_mid_query_returns_typed_timeout_through_the_boundary() {
    let store = common::TestStore::new();
    assert_eq!(
        common::ingest_rows(store.handle, 100, 8_192),
        ZeErrorCode::Ok
    );
    let vector = vec![0.25_f32; 8_192];
    let mut request = common::valid_search_request(&vector);
    request.k = 100;
    request.deadline_ns = 1;
    let mut result: ZeSearchResult = common::sized_zeroed();
    assert_eq!(
        ze_search(store.handle, &request, &mut result),
        ZeErrorCode::Timeout
    );
    assert_eq!(result.hit_count, 0);
    assert!(result.hits.is_null());
}

#[test]
fn search_result_ownership_and_the_phase_one_engine_seams_are_real() {
    let store = common::TestStore::new();
    let mut state: ZeStateReport = common::sized_zeroed();
    assert_eq!(ze_state(store.handle, &mut state), ZeErrorCode::Ok);
    assert_eq!(state.state, 0);
    let mut stats: ZeStatsReport = common::sized_zeroed();
    assert_eq!(ze_stats(store.handle, &mut stats), ZeErrorCode::Ok);

    assert_eq!(common::ingest_rows(store.handle, 2, 4), ZeErrorCode::Ok);
    let deleted = [ZeDocId { high: 0, low: 1 }];
    let delete = ZeDeleteRequest {
        abi_size: size_of::<ZeDeleteRequest>() as u32,
        abi_reserved: 0,
        doc_ids: deleted.as_ptr(),
        doc_id_count: deleted.len(),
    };
    let mut mutation: ZeMutationReport = common::sized_zeroed();
    assert_eq!(
        ze_delete(store.handle, &delete, &mut mutation),
        ZeErrorCode::Ok
    );
    let vector = vec![0.25_f32; 4];
    let request = common::valid_search_request(&vector);
    let mut result: ZeSearchResult = common::sized_zeroed();
    assert_eq!(
        ze_search(store.handle, &request, &mut result),
        ZeErrorCode::Ok
    );
    assert_eq!(result.hit_count, 1);
    assert!(!result.hits.is_null());
    assert_eq!(ze_search_result_free(&mut result), ZeErrorCode::Ok);
    assert_eq!(ze_search_result_free(&mut result), ZeErrorCode::Ok);
    let mut zeroed: ZeSearchResult = unsafe { std::mem::zeroed() };
    assert_eq!(ze_search_result_free(&mut zeroed), ZeErrorCode::Ok);

    let seal = ZeSealRequest {
        abi_size: size_of::<ZeSealRequest>() as u32,
        abi_reserved: 0,
        cancel_token: 0,
    };
    let mut generation: ZeGenerationReport = common::sized_zeroed();
    assert_eq!(
        ze_seal(store.handle, &seal, &mut generation),
        ZeErrorCode::Ok
    );

    let drop_request = ZeDropPartitionRequest {
        abi_size: size_of::<ZeDropPartitionRequest>() as u32,
        abi_reserved: 0,
        start_ts: i64::MIN,
        end_ts: 1,
    };
    let mut partition: ZePartitionReport = common::sized_zeroed();
    assert_eq!(
        ze_drop_partition(store.handle, &drop_request, &mut partition),
        ZeErrorCode::Ok
    );

    let retention = ZeRetentionRequest {
        abi_size: size_of::<ZeRetentionRequest>() as u32,
        abi_reserved: 0,
        window: 10,
        now_ts: 10,
    };
    assert_eq!(
        ze_apply_retention(store.handle, &retention, &mut partition),
        ZeErrorCode::Ok
    );

    let ids = [ZeDocId {
        high: u64::MAX,
        low: u64::MAX,
    }];
    let purge = ZePurgeRequest {
        abi_size: size_of::<ZePurgeRequest>() as u32,
        abi_reserved: 0,
        doc_ids: ids.as_ptr(),
        doc_id_count: ids.len(),
    };
    let mut token: ZePurgeTokenReport = common::sized_zeroed();
    assert_eq!(ze_purge(store.handle, &purge, &mut token), ZeErrorCode::Ok);
    assert_eq!(token.is_no_op, 1);
    let await_request = ZeAwaitPurgeRequest {
        abi_size: size_of::<ZeAwaitPurgeRequest>() as u32,
        abi_reserved: 0,
        token_id: token.token_id,
    };
    let mut purge_report: ZePurgeReport = common::sized_zeroed();
    assert_eq!(
        ze_await_physical_purge(store.handle, &await_request, &mut purge_report),
        ZeErrorCode::Ok
    );

    let maintain = ZeMaintainRequest {
        abi_size: size_of::<ZeMaintainRequest>() as u32,
        abi_reserved: 0,
        wall_time_ns: 0,
        bytes: 0,
    };
    let mut maintenance: ZeMaintainReport = common::sized_zeroed();
    assert_eq!(
        ze_maintain(store.handle, &maintain, &mut maintenance),
        ZeErrorCode::Ok
    );
    assert_eq!(maintenance.status, 1);
}
