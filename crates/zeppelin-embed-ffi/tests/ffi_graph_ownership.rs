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
            ze_graph_apply(
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
