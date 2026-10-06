//! Feature-gated allocator attribution used only by the accounting audit test.

use std::alloc::{GlobalAlloc, Layout, System};
use std::cell::Cell;

thread_local! {
    static ENGINE_DEPTH: Cell<u32> = const { Cell::new(0) };
    static ATTRIBUTED_DEPTH: Cell<u32> = const { Cell::new(0) };
    static ATTRIBUTED_BYTES: Cell<u64> = const { Cell::new(0) };
    static UNATTRIBUTED_BYTES: Cell<u64> = const { Cell::new(0) };
    static ALLOCATION_COUNT: Cell<u64> = const { Cell::new(0) };
    static FULL_SEGMENT_CLONES: Cell<u64> = const { Cell::new(0) };
}

struct AuditingAllocator;

#[global_allocator]
static GLOBAL_ALLOCATOR: AuditingAllocator = AuditingAllocator;

fn refuse_allocation() -> bool {
    #[cfg(any(test, feature = "test-seams"))]
    {
        if ATTRIBUTED_DEPTH.with(Cell::get) != 0 {
            return FAIL_AFTER.with(|remaining| {
                let value = remaining.get();
                if value == 0 {
                    return false;
                }
                remaining.set(value - 1);
                if value == 1 {
                    FAULT_FIRES.with(|fires| fires.set(fires.get() + 1));
                    true
                } else {
                    false
                }
            });
        }
    }
    false
}

unsafe impl GlobalAlloc for AuditingAllocator {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        if refuse_allocation() {
            return std::ptr::null_mut();
        }
        let pointer = unsafe {
            // SAFETY: this forwards the caller's valid `GlobalAlloc` layout unchanged.
            System.alloc(layout)
        };
        if !pointer.is_null() {
            record_allocation(layout.size());
        }
        pointer
    }

    unsafe fn alloc_zeroed(&self, layout: Layout) -> *mut u8 {
        if refuse_allocation() {
            return std::ptr::null_mut();
        }
        let pointer = unsafe {
            // SAFETY: this forwards the caller's valid `GlobalAlloc` layout unchanged.
            System.alloc_zeroed(layout)
        };
        if !pointer.is_null() {
            record_allocation(layout.size());
        }
        pointer
    }

    unsafe fn dealloc(&self, pointer: *mut u8, layout: Layout) {
        unsafe {
            // SAFETY: this forwards the exact pointer/layout pair supplied by the caller.
            System.dealloc(pointer, layout);
        }
    }

    unsafe fn realloc(&self, pointer: *mut u8, layout: Layout, new_size: usize) -> *mut u8 {
        if refuse_allocation() {
            return std::ptr::null_mut();
        }
        let new_pointer = unsafe {
            // SAFETY: this forwards the caller's valid allocation and requested new size.
            System.realloc(pointer, layout, new_size)
        };
        if !new_pointer.is_null() {
            record_allocation(new_size);
        }
        new_pointer
    }
}

fn record_allocation(bytes: usize) {
    ENGINE_DEPTH.with(|engine_depth| {
        if engine_depth.get() == 0 {
            return;
        }
        let amount = u64::try_from(bytes).unwrap_or(u64::MAX);
        ALLOCATION_COUNT.with(|count| count.set(count.get().saturating_add(1)));
        ATTRIBUTED_DEPTH.with(|attributed_depth| {
            let counter = if attributed_depth.get() == 0 {
                &UNATTRIBUTED_BYTES
            } else {
                &ATTRIBUTED_BYTES
            };
            counter.with(|counter| counter.set(counter.get().saturating_add(amount)));
        });
    });
}

struct DepthGuard {
    depth: &'static std::thread::LocalKey<Cell<u32>>,
}

impl DepthGuard {
    fn enter(depth: &'static std::thread::LocalKey<Cell<u32>>) -> Self {
        depth.with(|value| value.set(value.get().saturating_add(1)));
        Self { depth }
    }
}

impl Drop for DepthGuard {
    fn drop(&mut self) {
        self.depth
            .with(|value| value.set(value.get().saturating_sub(1)));
    }
}

#[cfg(any(test, feature = "test-seams"))]
/// Actual allocator counts for one scoped calling thread.
pub struct AuditReport {
    /// Successfully attributed allocated bytes.
    pub attributed_bytes: u64,
    /// Successfully allocated bytes outside a reservation-attributed site.
    pub unattributed_bytes: u64,
    /// Number of allocator calls, not their size.
    ///
    /// Bytes answer "is this accounted"; the count answers "does this loop
    /// allocate per item", which is the query-path gate in plan P0.4.
    pub allocations: u64,
    /// Full immutable segment copies seen by this path.
    pub full_segment_clones: u64,
}

#[cfg(any(test, feature = "test-seams"))]
/// Audits real allocation calls in one bounded synchronous engine operation.
pub fn audit_engine_path<T>(operation: impl FnOnce() -> T) -> (T, AuditReport) {
    ATTRIBUTED_DEPTH.with(|depth| depth.set(0));
    ATTRIBUTED_BYTES.with(|bytes| bytes.set(0));
    UNATTRIBUTED_BYTES.with(|bytes| bytes.set(0));
    ALLOCATION_COUNT.with(|count| count.set(0));
    FULL_SEGMENT_CLONES.with(|count| count.set(0));
    let engine = DepthGuard::enter(&ENGINE_DEPTH);
    let result = operation();
    drop(engine);
    let attributed_bytes = ATTRIBUTED_BYTES.with(Cell::get);
    let unattributed_bytes = UNATTRIBUTED_BYTES.with(Cell::get);
    let allocations = ALLOCATION_COUNT.with(Cell::get);
    let full_segment_clones = FULL_SEGMENT_CLONES.with(Cell::get);
    (
        result,
        AuditReport {
            attributed_bytes,
            unattributed_bytes,
            allocations,
            full_segment_clones,
        },
    )
}

pub(crate) fn record_full_segment_clone() {
    ENGINE_DEPTH.with(|depth| {
        if depth.get() > 0 {
            FULL_SEGMENT_CLONES.with(|count| count.set(count.get().saturating_add(1)));
        }
    });
}

pub(crate) fn attributed<T>(operation: impl FnOnce() -> T) -> T {
    let guard = DepthGuard::enter(&ATTRIBUTED_DEPTH);
    let result = operation();
    drop(guard);
    result
}

#[cfg(any(test, feature = "test-seams"))]
thread_local! {
    static FAIL_AFTER: Cell<u64> = const { Cell::new(0) };
    static FAULT_FIRES: Cell<u64> = const { Cell::new(0) };
}
#[cfg(any(test, feature = "test-seams"))]
struct FaultGuard {
    remaining: u64,
    fires: u64,
}
#[cfg(any(test, feature = "test-seams"))]
impl Drop for FaultGuard {
    fn drop(&mut self) {
        FAIL_AFTER.with(|value| value.set(self.remaining));
        FAULT_FIRES.with(|value| value.set(self.fires));
    }
}
/// Refuses the selected actual attributed allocation call on this thread.
/// Intended only around fallible allocation paths; zero disables injection.
/// State restores on return/unwind, and the returned count proves CAN FIRE.
#[cfg(any(test, feature = "test-seams"))]
pub fn fail_attributed_allocation<T>(ordinal: u64, operation: impl FnOnce() -> T) -> (T, u64) {
    let guard = FaultGuard {
        remaining: FAIL_AFTER.with(|v| v.replace(ordinal)),
        fires: FAULT_FIRES.with(|v| v.replace(0)),
    };
    let result = operation();
    let fires = FAULT_FIRES.with(Cell::get);
    drop(guard);
    (result, fires)
}
