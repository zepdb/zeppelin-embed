//! Real allocator observations over the complete compiler/lowerer owner overlap.
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
use std::{
    alloc::{GlobalAlloc, Layout, System},
    cell::Cell,
    ptr,
};
use zeppelin_embed::lifecycle::{CancelToken, OpenOptions, QueryControl, Store};
use zeppelin_embed::property_graph::{
    GraphGeneration, StoreInstanceId,
    query::{QueryView, ValueContext, resources::*},
    resources::GraphResources,
};
use zeppelin_embed_cypher::{CompileLimits, ErrorKind, ResourceError, compile_read_in};
thread_local! {
    static TRACK:Cell<bool>=const {Cell::new(false)};
    static MEMORY:Cell<*const QueryMemory<'static>>=const {Cell::new(ptr::null())};
    static BASE:Cell<usize>=const {Cell::new(0)};
    static CALLS:Cell<usize>=const {Cell::new(0)};
    static FAIL:Cell<usize>=const {Cell::new(0)};
    static FIRED:Cell<usize>=const {Cell::new(0)};
    static LIVE:Cell<usize>=const {Cell::new(0)};
    static PEAK:Cell<usize>=const {Cell::new(0)};
    static UNDER:Cell<bool>=const {Cell::new(false)};
}
struct Observed;
#[global_allocator]
static ALLOCATOR: Observed = Observed;
fn admit(size: usize) -> bool {
    if !TRACK.get() {
        return true;
    }
    let calls = CALLS.get() + 1;
    CALLS.set(calls);
    if calls == FAIL.get() {
        FIRED.set(FIRED.get() + 1);
        return false;
    }
    let live = LIVE.get() + size;
    LIVE.set(live);
    PEAK.set(PEAK.get().max(live));
    // Test-only synchronous pointer: the QueryMemory lives outside the active
    // interval on this exact thread. Reading its Cell does not allocate.
    let reserved = unsafe { (&*MEMORY.get()).reserved_bytes() };
    if live > reserved - BASE.get() {
        UNDER.set(true);
    }
    true
}
unsafe impl GlobalAlloc for Observed {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        if !admit(layout.size()) {
            return ptr::null_mut();
        }
        unsafe { System.alloc(layout) }
    }
    unsafe fn dealloc(&self, pointer: *mut u8, layout: Layout) {
        if TRACK.get() {
            LIVE.set(LIVE.get() - layout.size());
        }
        unsafe { System.dealloc(pointer, layout) }
    }
    unsafe fn realloc(&self, pointer: *mut u8, layout: Layout, size: usize) -> *mut u8 {
        if !admit(size) {
            return ptr::null_mut();
        }
        let output = unsafe { System.realloc(pointer, layout, size) };
        if TRACK.get() {
            LIVE.set(LIVE.get() - layout.size());
        }
        output
    }
}
fn start(memory: &QueryMemory<'_>, fail: usize) {
    MEMORY.set((memory as *const QueryMemory<'_>).cast());
    BASE.set(memory.reserved_bytes());
    CALLS.set(0);
    FAIL.set(fail);
    FIRED.set(0);
    LIVE.set(0);
    PEAK.set(0);
    UNDER.set(false);
    TRACK.set(true);
}
fn finish() -> (usize, usize, usize) {
    TRACK.set(false);
    assert_eq!(LIVE.get(), 0);
    assert!(!UNDER.get(), "allocator saw unreserved live overlap");
    (CALLS.get(), FIRED.get(), PEAK.get())
}
#[test]
fn read_lowering_real_allocator_fail_at_each_site_releases_all_backing() {
    let path = std::env::temp_dir().join(format!("ze126-allocator-{}", std::process::id()));
    std::fs::create_dir(&path).unwrap();
    let store = Store::open(
        &path,
        OpenOptions::new().with_max_resident_bytes(64 * 1024 * 1024),
    )
    .unwrap();
    let shared = GraphResources::from_store(&store).unwrap();
    {
        let memory = QueryMemory::new(&shared, 24 * 1024 * 1024).unwrap();
        let control = QueryControl::Cancel(CancelToken::new());
        let view = QueryView::new(StoreInstanceId::new(1).unwrap(), GraphGeneration::new(0));
        let mut context = ValueContext::new(&view, &control, 8_000_000).unwrap();
        let query = "MATCH (a:A:B {x:7})-[r:R|S]->(b) OPTIONAL MATCH (b)-[p:P*0..2 {weight:9}]->(c) WHERE c.ok=true WITH a,b,c,p,[1,2,3,'λ'] AS values ORDER BY b.x RETURN a,b,c,p,values";
        let baseline = memory.reserved_bytes();
        start(&memory, 0);
        let result = compile_read_in(
            query,
            &[],
            CompileLimits::default(),
            &memory,
            &mut context,
            |_, _| Ok(()),
        );
        let (calls, fires, peak) = finish();
        result.unwrap();
        assert_eq!(fires, 0);
        assert!(calls > 100);
        assert_eq!(memory.reserved_bytes(), baseline);
        for site in 1..=calls {
            let mut entered = false;
            start(&memory, site);
            let result = compile_read_in(
                query,
                &[],
                CompileLimits::default(),
                &memory,
                &mut context,
                |_, _| {
                    entered = true;
                    Ok(())
                },
            );
            let (_, fires, _) = finish();
            assert_eq!(fires, 1, "allocation site {site}");
            assert!(!entered, "allocation site {site}");
            assert!(
                matches!(
                    result,
                    Err(zeppelin_embed_cypher::ParseError {
                        kind: ErrorKind::Resource(ResourceError::Allocation),
                        ..
                    })
                ),
                "site {site}: {result:?}"
            );
            assert_eq!(memory.reserved_bytes(), baseline, "allocation site {site}");
        }
        start(&memory, 0);
        let result = compile_read_in(
            query,
            &[],
            CompileLimits::default(),
            &memory,
            &mut context,
            |_, _| Ok(()),
        );
        let (restored, _, _) = finish();
        result.unwrap();
        assert_eq!(restored, calls);
        eprintln!(
            "allocator_sites={calls} real_heap_peak={peak} query_reservation_peak={} final={}",
            memory.peak_reserved_bytes(),
            memory.reserved_bytes()
        );
    }
    drop(shared);
    store.close().unwrap();
    drop(store);
    std::fs::remove_dir_all(path).unwrap();
}

#[test]
fn search_lowering_real_allocator_fail_at_each_site_releases_all_backing() {
    let path = std::env::temp_dir().join(format!("ze138-allocator-{}", std::process::id()));
    std::fs::create_dir(&path).unwrap();
    let store = Store::open(
        &path,
        OpenOptions::new().with_max_resident_bytes(64 * 1024 * 1024),
    )
    .unwrap();
    let shared = GraphResources::from_store(&store).unwrap();
    {
        let memory = QueryMemory::new(&shared, 24 * 1024 * 1024).unwrap();
        let control = QueryControl::Cancel(CancelToken::new());
        let view = QueryView::new(StoreInstanceId::new(1).unwrap(), GraphGeneration::new(0));
        let mut context = ValueContext::new(&view, &control, 8_000_000).unwrap();
        let query = "MATCH (prior) WITH prior,1+1 AS k CALL ze.vector_search([k,2],k,'exact',[]) YIELD node,distance CALL ze.hybrid_search([k,2],'x',k,'auto') YIELD node AS other,score,vector_distance,lexical_score RETURN prior,node,distance,other,score,vector_distance,lexical_score LIMIT 0";
        let baseline = memory.reserved_bytes();
        start(&memory, 0);
        let result = compile_read_in(
            query,
            &[],
            CompileLimits::default(),
            &memory,
            &mut context,
            |_, _| Ok(()),
        );
        let (calls, fires, peak) = finish();
        result.unwrap();
        assert_eq!(fires, 0);
        assert!(calls > 100);
        assert_eq!(memory.reserved_bytes(), baseline);
        for site in 1..=calls {
            let mut entered = false;
            start(&memory, site);
            let result = compile_read_in(
                query,
                &[],
                CompileLimits::default(),
                &memory,
                &mut context,
                |_, _| {
                    entered = true;
                    Ok(())
                },
            );
            let (_, fires, _) = finish();
            assert_eq!(fires, 1, "allocation site {site}");
            assert!(!entered, "allocation site {site}");
            assert!(
                matches!(
                    result,
                    Err(zeppelin_embed_cypher::ParseError {
                        kind: ErrorKind::Resource(ResourceError::Allocation),
                        ..
                    })
                ),
                "site {site}: {result:?}"
            );
            assert_eq!(memory.reserved_bytes(), baseline, "allocation site {site}");
        }
        start(&memory, 0);
        let result = compile_read_in(
            query,
            &[],
            CompileLimits::default(),
            &memory,
            &mut context,
            |_, _| Ok(()),
        );
        let (restored, _, _) = finish();
        result.unwrap();
        assert_eq!(restored, calls);
        eprintln!(
            "search_allocator_sites={calls} real_heap_peak={peak} query_reservation_peak={} final={}",
            memory.peak_reserved_bytes(),
            memory.reserved_bytes()
        );
    }
    drop(shared);
    store.close().unwrap();
    drop(store);
    std::fs::remove_dir_all(path).unwrap();
}
