//! Independent allocator observation: reservation precedes every real allocation.
use std::alloc::{GlobalAlloc, Layout, System};
use std::cell::Cell;
use zeppelin_embed_cypher::{CompileLimits, ResourceError, Resources, compile_with};
thread_local! {
    static ACTIVE: Cell<bool> = const { Cell::new(false) };
    static CALLS: Cell<usize> = const { Cell::new(0) };
    static LIVE: Cell<usize> = const { Cell::new(0) };
    static CREDIT: Cell<usize> = const { Cell::new(0) };
    static UNDER: Cell<bool> = const { Cell::new(false) };
}
struct Observed;
#[global_allocator]
static ALLOCATOR: Observed = Observed;
unsafe impl GlobalAlloc for Observed {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        if ACTIVE.with(Cell::get) {
            CALLS.with(|cell| cell.set(cell.get() + 1));
            let live = LIVE.with(|cell| {
                let live = cell.get() + layout.size();
                cell.set(live);
                live
            });
            if live > CREDIT.with(Cell::get) {
                UNDER.with(|cell| cell.set(true));
            }
        }
        unsafe { System.alloc(layout) }
    }
    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        if ACTIVE.with(Cell::get) {
            LIVE.with(|cell| cell.set(cell.get() - layout.size()));
        }
        unsafe { System.dealloc(ptr, layout) }
    }
    unsafe fn realloc(&self, ptr: *mut u8, layout: Layout, size: usize) -> *mut u8 {
        if ACTIVE.with(Cell::get) {
            // System realloc may retain the old block while allocating its replacement.
            if LIVE.with(Cell::get) + size > CREDIT.with(Cell::get) {
                UNDER.with(|cell| cell.set(true));
            }
            LIVE.with(|cell| cell.set(cell.get() + size));
            LIVE.with(|cell| cell.set(cell.get() - layout.size()));
        }
        unsafe { System.realloc(ptr, layout, size) }
    }
}
struct Credits;
impl Resources for Credits {
    fn charge(&mut self, bytes: usize) -> Result<(), ResourceError> {
        CREDIT.with(|cell| cell.set(cell.get() + bytes));
        Ok(())
    }
    fn checkpoint(&mut self) -> Result<(), ResourceError> {
        Ok(())
    }
}
#[test]
fn compiler_reserves_real_growth_overlap_before_allocator_calls() {
    ACTIVE.with(|cell| cell.set(true));
    let result = compile_with(
        "MATCH (a:A), (b:B) WITH a, b, [1,2,3,4,5,6,7,8,9] AS values RETURN a, b, values",
        &[],
        CompileLimits::default(),
        &mut Credits,
        |_| Ok(()),
    );
    ACTIVE.with(|cell| cell.set(false));
    assert!(result.is_ok());
    assert_eq!(
        LIVE.with(Cell::get),
        0,
        "all compiler backing freed before its account"
    );
    assert!(
        !UNDER.with(Cell::get),
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
    ACTIVE.with(|cell| cell.set(true));
    let result = compile_with("RETURN 1", &[], CompileLimits::default(), &mut Deny, |_| {
        Ok(())
    });
    ACTIVE.with(|cell| cell.set(false));
    assert_eq!(
        result.unwrap_err().kind,
        zeppelin_embed_cypher::ErrorKind::Resource(ResourceError::Memory)
    );
    assert_eq!(CALLS.with(Cell::get), 0);
    assert_eq!(LIVE.with(Cell::get), 0);
}
