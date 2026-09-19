//! Independent allocator observation: reservation precedes every real allocation.
use std::alloc::{GlobalAlloc, Layout, System};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering::SeqCst};
use zeppelin_embed_cypher::{CompileLimits, ResourceError, Resources, compile_with};
static ACTIVE: AtomicBool = AtomicBool::new(false);
static CALLS: AtomicUsize = AtomicUsize::new(0);
static LIVE: AtomicUsize = AtomicUsize::new(0);
static CREDIT: AtomicUsize = AtomicUsize::new(0);
static UNDER: AtomicBool = AtomicBool::new(false);
struct Observed;
#[global_allocator]
static ALLOCATOR: Observed = Observed;
unsafe impl GlobalAlloc for Observed {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        if ACTIVE.load(SeqCst) {
            CALLS.fetch_add(1, SeqCst);
            let live = LIVE.fetch_add(layout.size(), SeqCst) + layout.size();
            if live > CREDIT.load(SeqCst) {
                UNDER.store(true, SeqCst);
            }
        }
        unsafe { System.alloc(layout) }
    }
    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        if ACTIVE.load(SeqCst) {
            LIVE.fetch_sub(layout.size(), SeqCst);
        }
        unsafe { System.dealloc(ptr, layout) }
    }
    unsafe fn realloc(&self, ptr: *mut u8, layout: Layout, size: usize) -> *mut u8 {
        if ACTIVE.load(SeqCst) {
            // System realloc may retain the old block while allocating its replacement.
            if LIVE.load(SeqCst) + size > CREDIT.load(SeqCst) {
                UNDER.store(true, SeqCst);
            }
            LIVE.fetch_add(size, SeqCst);
            LIVE.fetch_sub(layout.size(), SeqCst);
        }
        unsafe { System.realloc(ptr, layout, size) }
    }
}
struct Credits;
impl Resources for Credits {
    fn charge(&mut self, bytes: usize) -> Result<(), ResourceError> {
        CREDIT.fetch_add(bytes, SeqCst);
        Ok(())
    }
    fn checkpoint(&mut self) -> Result<(), ResourceError> {
        Ok(())
    }
}
#[test]
fn compiler_reserves_real_growth_overlap_before_allocator_calls() {
    ACTIVE.store(true, SeqCst);
    let result = compile_with(
        "MATCH (a:A), (b:B) WITH a, b, [1,2,3,4,5,6,7,8,9] AS values RETURN a, b, values",
        &[],
        CompileLimits::default(),
        &mut Credits,
        |_| Ok(()),
    );
    ACTIVE.store(false, SeqCst);
    assert!(result.is_ok());
    assert_eq!(
        LIVE.load(SeqCst),
        0,
        "all compiler backing freed before its account"
    );
    assert!(
        !UNDER.load(SeqCst),
        "real allocation/reallocation exceeded prior reservation"
    );
}

struct Deny;
impl Resources for Deny {
    fn charge(&mut self, _: usize) -> Result<(), ResourceError> {
        Err(ResourceError::Memory)
    }
    fn checkpoint(&mut self) -> Result<(), ResourceError> {
        Ok(())
    }
}
#[test]
fn compiler_budget_denial_precedes_the_first_allocator_call() {
    ACTIVE.store(true, SeqCst);
    let result = compile_with("RETURN 1", &[], CompileLimits::default(), &mut Deny, |_| {
        Ok(())
    });
    ACTIVE.store(false, SeqCst);
    assert_eq!(
        result.unwrap_err().kind,
        zeppelin_embed_cypher::ErrorKind::Resource(ResourceError::Memory)
    );
    assert_eq!(CALLS.load(SeqCst), 0);
    assert_eq!(LIVE.load(SeqCst), 0);
}
