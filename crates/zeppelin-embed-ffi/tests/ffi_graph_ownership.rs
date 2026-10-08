//! Callee-owned buffers must be released by their matching `ze_*_free`, and
//! an allocate/free loop must leave the process heap exactly flat.

mod common;

use common::graph::*;
use std::alloc::{GlobalAlloc, Layout, System};
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
fn graph_apply_and_free_loops_keep_the_heap_flat() {
    let _guard = HEAP_TEST_GUARD.lock().unwrap();
    let mut round = 0;
    // Each fresh-key write retains legitimate store state. Close and drop the
    // store in each measured round so the delta isolates leaked ownership,
    // including response arenas, rather than graph contents and caches.
    assert_heap_flat("graph apply/free", || {
        let mut store = GraphTestStore::create();
        let mut b = PoolBuilder::new();
        let item = single_node(&mut b, &format!("node-{round}"));
        round += 1;
        let mut response = empty_response();
        assert_eq!(
            ze_store_graph_apply(
                store.handle,
                &batch_request(&[item], &b.pool()),
                &mut response
            ),
            ZeErrorCode::ZeOk
        );
        assert_eq!(ze_graph_response_free(&mut response), ZeErrorCode::ZeOk);
        assert_eq!(store.close(), ZeErrorCode::ZeOk);
    });
}

#[test]
fn graph_cypher_responses_free_in_a_flat_heap_loop() {
    let _guard = HEAP_TEST_GUARD.lock().unwrap();
    let s = GraphTestStore::create();
    let mut r = cypher_ok(s.handle, "CREATE (:Doc {title:'alpha'})");
    ze_graph_response_free(&mut r);
    assert_heap_flat("cypher read/free", || {
        let mut r = cypher_ok(s.handle, "MATCH (n:Doc) RETURN n.title AS title");
        assert_eq!(ze_graph_response_free(&mut r), ZeErrorCode::ZeOk);
    });
}

#[test]
fn ze241_query_and_get_responses_free_in_a_flat_heap_loop() {
    let _guard = HEAP_TEST_GUARD.lock().unwrap();
    let store = GraphTestStore::create();
    let mut created = cypher_ok(store.handle, "CREATE (n:Owned) RETURN n");
    assert_eq!(created.pool.node_count, 1);
    let node = unsafe { (*created.pool.nodes).id };
    ze_graph_response_free(&mut created);
    let ids = [
        node,
        ZeNodeId {
            high: u64::MAX,
            low: node.low,
        },
        node,
    ];
    let mut get: ZeGraphGetNodesRequest = common::sized_zeroed();
    get.ids = ids.as_ptr();
    get.id_count = ids.len();
    let rels = [ZeRelId {
        high: u64::MAX,
        low: 1,
    }];
    let mut relationships: ZeGraphGetRelsRequest = common::sized_zeroed();
    relationships.ids = rels.as_ptr();
    relationships.id_count = 1;
    let mut operator: ZeGraphOperator = common::sized_zeroed();
    operator.kind = 4;
    operator.node_slot = 7;
    let pool: ZeGraphValuePool = common::sized_zeroed();
    let mut plan: ZeGraphPlan = common::sized_zeroed();
    plan.operators = &operator;
    plan.operator_count = 1;
    plan.pool = &pool;
    let mut query: ZeGraphQueryRequest = common::sized_zeroed();
    query.plan = &plan;
    assert_heap_flat("structured query/gets/free", || {
        let mut response = empty_response();
        assert_eq!(
            ze_store_graph_query(store.handle, &query, &mut response),
            ZeErrorCode::ZeOk
        );
        assert_eq!(ze_graph_response_free(&mut response), ZeErrorCode::ZeOk);
        assert_eq!(
            ze_store_get_nodes(store.handle, &get, &mut response),
            ZeErrorCode::ZeOk
        );
        assert_eq!(ze_graph_response_free(&mut response), ZeErrorCode::ZeOk);
        assert_eq!(
            ze_store_get_relationships(store.handle, &relationships, &mut response),
            ZeErrorCode::ZeOk
        );
        assert_eq!(ze_graph_response_free(&mut response), ZeErrorCode::ZeOk);
    });
}

#[test]
fn ze72_nested_search_error_and_retained_responses_keep_heap_flat() {
    let _guard = HEAP_TEST_GUARD.lock().unwrap();
    assert_heap_flat("ZE-72 nested/entity/search/error/retained", || {
        let mut store = GraphTestStore::create();
        let mut b = PoolBuilder::new();
        let ns = b.text("ze72");
        let key = b.text("owned");
        let image = b.node_image(&["Owned"], 0..0, Some("amber"));
        let pool = b.pool();
        let mut created = empty_response();
        assert_eq!(
            ze_store_graph_apply(
                store.handle,
                &batch_request(&[create_node_item(ns, key, 1, image)], &pool),
                &mut created
            ),
            ZeErrorCode::ZeOk
        );
        let id = receipts(&created)[0].node;
        ze_graph_response_free(&mut created);
        bind_search_documents(
            &mut store.handle,
            &store.path,
            None,
            &[(id, Some("amber"), [0.0, 0.0])],
            &[],
        );
        let mut nested = cypher_ok(store.handle, "MATCH (n:Owned) RETURN n,[n,null,[n]],[]");
        let mut search = cypher_ok(
            store.handle,
            "CALL ze.text_search('amber',2) YIELD node,score RETURN node,score",
        );
        assert_eq!(search.report_count, 1);
        assert_eq!(search.row_count, 1);
        let mut error = empty_response();
        assert_eq!(
            ze_store_cypher(
                store.handle,
                &cypher_request(b"RETURN sin(1)", &[], None),
                &mut error
            ),
            ZeErrorCode::ZeErrQueryUnsupported
        );
        assert_eq!(ze_graph_response_free(&mut error), ZeErrorCode::ZeOk);
        assert_eq!(store.close(), ZeErrorCode::ZeOk);
        let (code, reopened) = graph_open(&store.path, MODE_READ_WRITE);
        assert_eq!(code, ZeErrorCode::ZeOk);
        assert_eq!(rows(&nested)[0][0].tag, 5);
        assert_eq!(rows(&search)[0][0].tag, 5);
        assert_eq!(ze_graph_response_free(&mut nested), ZeErrorCode::ZeOk);
        assert_eq!(ze_graph_response_free(&mut search), ZeErrorCode::ZeOk);
        assert_eq!(ze_close(reopened), ZeErrorCode::ZeOk);
    });
}

#[test]
fn ze76_public_resources_report_engine_capacities() {
    let _guard = HEAP_TEST_GUARD.lock().unwrap();
    let store = GraphTestStore::create();
    let mut observation = ZeGraphResources {
        abi_size: std::mem::size_of::<ZeGraphResources>() as u32,
        ..ZeGraphResources::default()
    };
    assert_eq!(
        ze_store_graph_resources(store.handle, &mut observation),
        ZeErrorCode::ZeOk
    );
    assert!(observation.engine_bytes > 0);
    assert!(observation.engine_peak_bytes >= observation.engine_bytes);
    assert_eq!(
        (
            observation.application_bytes,
            observation.application_peak_bytes
        ),
        (0, 0)
    );
    let mut invalid = observation;
    invalid.abi_reserved = 1;
    assert_eq!(
        ze_store_graph_resources(store.handle, &mut invalid),
        ZeErrorCode::ZeErrInvalidArgument
    );
    assert_eq!(invalid.engine_bytes, observation.engine_bytes);
}
