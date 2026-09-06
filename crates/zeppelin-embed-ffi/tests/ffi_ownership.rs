//! Callee-owned buffers must be released by their matching `ze_*_free`, and
//! an allocate/free loop must leave the process heap exactly flat.

mod common;

use std::alloc::{GlobalAlloc, Layout, System};
use std::mem::size_of;
use std::sync::Mutex;
use std::sync::atomic::{AtomicUsize, Ordering};

use zeppelin_embed_ffi::*;

struct CountingAllocator;

static LIVE_BYTES: AtomicUsize = AtomicUsize::new(0);
static HEAP_TEST_GUARD: Mutex<()> = Mutex::new(());

unsafe impl GlobalAlloc for CountingAllocator {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        let pointer = unsafe { System.alloc(layout) };
        if !pointer.is_null() {
            LIVE_BYTES.fetch_add(layout.size(), Ordering::SeqCst);
        }
        pointer
    }

    unsafe fn dealloc(&self, pointer: *mut u8, layout: Layout) {
        LIVE_BYTES.fetch_sub(layout.size(), Ordering::SeqCst);
        unsafe { System.dealloc(pointer, layout) };
    }

    unsafe fn realloc(&self, pointer: *mut u8, layout: Layout, new_size: usize) -> *mut u8 {
        let moved = unsafe { System.realloc(pointer, layout, new_size) };
        if !moved.is_null() {
            LIVE_BYTES.fetch_sub(layout.size(), Ordering::SeqCst);
            LIVE_BYTES.fetch_add(new_size, Ordering::SeqCst);
        }
        moved
    }
}

#[global_allocator]
static ALLOCATOR: CountingAllocator = CountingAllocator;

const DIMENSION: usize = 8;
const WARMUP: usize = 16;
const ITERATIONS: usize = 256;

fn live_bytes() -> usize {
    LIVE_BYTES.load(Ordering::SeqCst)
}

/// Runs `round` repeatedly and asserts the heap is exactly flat after the
/// warm-up, so a callee-owned buffer returned without its free is visible as
/// growth proportional to the iteration count.
fn assert_heap_flat(name: &str, mut round: impl FnMut()) {
    for _ in 0..WARMUP {
        round();
    }
    let baseline = live_bytes();
    for _ in 0..ITERATIONS {
        round();
    }
    let after = live_bytes();
    assert_eq!(
        after,
        baseline,
        "{name}: heap grew by {} bytes over {ITERATIONS} allocate/free rounds",
        after as isize - baseline as isize
    );
}

#[test]
fn every_callee_owned_result_is_released_by_its_free_and_the_heap_stays_flat() {
    let _heap_guard = HEAP_TEST_GUARD.lock().expect("heap test guard");
    let store = common::TestStore::new();
    assert_eq!(
        common::ingest_rows(store.handle, 32, DIMENSION),
        ZeErrorCode::ZeOk
    );
    let probe = vec![0.5_f32; DIMENSION];

    let mut search_request = common::valid_search_request(&probe);
    search_request.k = 16;
    assert_heap_flat("ze_search/ze_search_result_free", || {
        let mut result: ZeSearchResult = common::sized_zeroed();
        assert_eq!(
            ze_search(store.handle, &search_request, &mut result),
            ZeErrorCode::ZeOk
        );
        assert_eq!(result.hit_count, 16);
        assert_eq!(ze_search_result_free(&mut result), ZeErrorCode::ZeOk);
    });

    let filter_node = ZeFilterNode {
        op: 6,
        attribute_id: 0,
        values: std::ptr::null(),
        value_count: 0,
        has_lower: 0,
        lower: unsafe { std::mem::zeroed() },
        lower_inclusive: 0,
        has_upper: 0,
        upper: unsafe { std::mem::zeroed() },
        upper_inclusive: 0,
        children_start: 0,
        children_count: 0,
    };
    let filter = ZeFilter {
        abi_size: size_of::<ZeFilter>() as u32,
        abi_reserved: 0,
        nodes: &filter_node,
        node_count: 1,
        root: 0,
    };
    let filtered_request = ZeSearchFilteredRequest {
        abi_size: size_of::<ZeSearchFilteredRequest>() as u32,
        abi_reserved: 0,
        search: search_request,
        filter: &filter,
    };
    assert_heap_flat("ze_search_filtered/ze_search_result_free", || {
        let mut result: ZeSearchResult = common::sized_zeroed();
        assert_eq!(
            ze_search_filtered(store.handle, &filtered_request, &mut result),
            ZeErrorCode::ZeOk
        );
        assert_eq!(result.hit_count, 16);
        assert_eq!(ze_search_result_free(&mut result), ZeErrorCode::ZeOk);
    });

    let mut query_request = common::valid_query_request(&probe);
    query_request.k = 16;
    assert_heap_flat("ze_query/ze_query_result_free", || {
        let mut result: ZeQueryResult = common::sized_zeroed();
        assert_eq!(
            ze_query(store.handle, &query_request, &mut result),
            ZeErrorCode::ZeOk
        );
        assert_eq!(result.hit_count, 16);
        assert_eq!(ze_query_result_free(&mut result), ZeErrorCode::ZeOk);
    });

    let get_ids = (1..=16)
        .map(|low| ZeDocId { high: 0, low })
        .collect::<Vec<_>>();
    let get_request = ZeGetRequest {
        abi_size: size_of::<ZeGetRequest>() as u32,
        abi_reserved: 0,
        ids: get_ids.as_ptr(),
        id_count: get_ids.len(),
        include_vector: 1,
        include_text: 1,
        include_metadata: 1,
        include_attributes: 1,
    };
    assert_heap_flat("ze_get/ze_get_result_free", || {
        let mut result: ZeGetResult = common::sized_zeroed();
        assert_eq!(
            ze_get(store.handle, &get_request, &mut result),
            ZeErrorCode::ZeOk
        );
        assert_eq!(result.document_count, get_ids.len());
        assert_eq!(ze_get_result_free(&mut result), ZeErrorCode::ZeOk);
    });

    let scan_request = ZeScanRequest {
        abi_size: size_of::<ZeScanRequest>() as u32,
        abi_reserved: 0,
        cursor_generation: 0,
        cursor_segment_id: [0; 16],
        cursor_next_row: 0,
        cursor_phase: 0,
        limit: 16,
        order: 0,
        include_vector: 1,
        include_text: 1,
        include_metadata: 1,
        include_attributes: 1,
        has_timestamp_range: 0,
        start_ts: 0,
        end_ts: 0,
        filter: std::ptr::null(),
        cancel_token: 0,
        deadline_ns: 0,
    };
    assert_heap_flat("ze_scan/ze_scan_result_free", || {
        let mut result: ZeScanResult = common::sized_zeroed();
        assert_eq!(
            ze_scan(store.handle, &scan_request, &mut result),
            ZeErrorCode::ZeOk
        );
        assert_eq!(result.document_count, 16);
        assert_eq!(ze_scan_result_free(&mut result), ZeErrorCode::ZeOk);
    });

    let namespace_root = tempfile::tempdir().expect("temporary namespace root");
    let root_bytes = namespace_root
        .path()
        .to_string_lossy()
        .into_owned()
        .into_bytes();
    let name = b"owned";
    let spec = ZeNamespaceSpec {
        abi_size: size_of::<ZeNamespaceSpec>() as u32,
        abi_reserved: 0,
        attributes: std::ptr::null(),
        attribute_count: 0,
        has_vector_space: 1,
        dimensions: DIMENSION as u32,
        normalization: 0,
        epoch: std::ptr::null(),
    };
    let open = ZeNamespaceOpenRequest {
        abi_size: size_of::<ZeNamespaceOpenRequest>() as u32,
        abi_reserved: 0,
        root: root_bytes.as_ptr(),
        root_len: root_bytes.len(),
        name: name.as_ptr(),
        name_len: name.len(),
        open: ZeOpenRequest {
            abi_size: size_of::<ZeOpenRequest>() as u32,
            abi_reserved: 0,
            path: std::ptr::null(),
            path_len: 0,
            access_mode: 0,
            durability_mode: 0,
            commit_tier: 1,
            reader_drain_timeout_ms: 250,
            max_resident_bytes: u64::MAX,
            max_temp_bytes: u64::MAX,
        },
        spec: &spec,
    };
    let mut namespace_handle = 0;
    assert_eq!(
        ze_namespace_open(&open, &mut namespace_handle),
        ZeErrorCode::ZeOk
    );
    assert_eq!(ze_close(namespace_handle), ZeErrorCode::ZeOk);
    let list = ZeNamespaceListRequest {
        abi_size: size_of::<ZeNamespaceListRequest>() as u32,
        abi_reserved: 0,
        root: root_bytes.as_ptr(),
        root_len: root_bytes.len(),
    };
    assert_heap_flat("ze_namespace_list/ze_namespace_list_result_free", || {
        let mut result: ZeNamespaceListResult = common::sized_zeroed();
        assert_eq!(ze_namespace_list(&list, &mut result), ZeErrorCode::ZeOk);
        assert_eq!(result.entry_count, 1);
        assert_eq!(
            ze_namespace_list_result_free(&mut result),
            ZeErrorCode::ZeOk
        );
    });

    assert_heap_flat("ze_cancel_token_create/ze_cancel_token_free", || {
        let mut token = 0;
        assert_eq!(ze_cancel_token_create(&mut token), ZeErrorCode::ZeOk);
        assert_eq!(ze_cancel_token_free(token), ZeErrorCode::ZeOk);
    });

    assert_heap_flat("ze_last_error_message size probe", || {
        let mut written = 0;
        let mut request = query_request;
        request.k = 0;
        let mut result: ZeQueryResult = common::sized_zeroed();
        assert_eq!(
            ze_query(store.handle, &request, &mut result),
            ZeErrorCode::ZeErrInvalidArgument
        );
        assert_eq!(
            ze_last_error_message(store.handle, std::ptr::null_mut(), 0, &mut written),
            ZeErrorCode::ZeOk
        );
        assert!(written > 0);
    });
}

#[test]
fn freeing_a_foreign_or_already_released_buffer_is_a_typed_error_not_a_double_free() {
    let _heap_guard = HEAP_TEST_GUARD.lock().expect("heap test guard");
    let store = common::TestStore::new();
    assert_eq!(
        common::ingest_rows(store.handle, 4, DIMENSION),
        ZeErrorCode::ZeOk
    );
    let probe = vec![0.5_f32; DIMENSION];
    let mut request = common::valid_query_request(&probe);
    request.k = 4;
    let mut result: ZeQueryResult = common::sized_zeroed();
    assert_eq!(
        ze_query(store.handle, &request, &mut result),
        ZeErrorCode::ZeOk
    );
    let hits = result.hits;
    let hit_count = result.hit_count;

    // A search-result free cannot release a query allocation: the element
    // size disagrees, so the registry refuses instead of deallocating.
    let mut forged: ZeSearchResult = common::sized_zeroed();
    forged.hits = hits.cast::<ZeSearchHit>();
    forged.hit_count = hit_count;
    assert_eq!(
        ze_search_result_free(&mut forged),
        ZeErrorCode::ZeErrInvalidArgument
    );
    assert_eq!(ze_query_result_free(&mut result), ZeErrorCode::ZeOk);

    let mut stale: ZeQueryResult = common::sized_zeroed();
    stale.hits = hits;
    stale.hit_count = hit_count;
    assert_eq!(
        ze_query_result_free(&mut stale),
        ZeErrorCode::ZeErrInvalidArgument
    );
    let mut foreign: ZeQueryResult = common::sized_zeroed();
    let mut buffer = vec![unsafe { std::mem::zeroed::<ZeQueryHit>() }; 2];
    foreign.hits = buffer.as_mut_ptr();
    foreign.hit_count = buffer.len();
    assert_eq!(
        ze_query_result_free(&mut foreign),
        ZeErrorCode::ZeErrInvalidArgument
    );
    assert_eq!(
        ze_query_result_free(std::ptr::null_mut()),
        ZeErrorCode::ZeErrInvalidArgument
    );

    let mut arena = [0_u64; 4];
    let mut foreign_namespace: ZeNamespaceListResult = common::sized_zeroed();
    foreign_namespace.entries = arena.as_mut_ptr().cast::<ZeNamespaceEntry>();
    foreign_namespace.entry_count = 1;
    assert_eq!(
        ze_namespace_list_result_free(&mut foreign_namespace),
        ZeErrorCode::ZeErrInvalidArgument
    );
    assert_eq!(
        ze_namespace_list_result_free(std::ptr::null_mut()),
        ZeErrorCode::ZeErrInvalidArgument
    );
}
