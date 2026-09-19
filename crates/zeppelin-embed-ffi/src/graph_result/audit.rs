//! Test-only thread-local system-allocator audit. Nextest executes each test in
//! its own process; assertions/formatting run only after auditing is disabled.
use std::alloc::{GlobalAlloc, Layout, System};
use std::cell::Cell;

#[derive(Clone, Copy, Debug, Default)]
pub(super) struct Snapshot {
    pub attempts: usize,
    pub allocations: usize,
    pub frees: usize,
    pub bytes: isize,
    pub peak: usize,
}
#[derive(Clone, Copy)]
struct State {
    enabled: bool,
    fail_at: usize,
    deny_all: bool,
    value: Snapshot,
}
thread_local! {
    static STATE: Cell<State> = const { Cell::new(State { enabled: false, fail_at: 0, deny_all: false, value: Snapshot { attempts: 0, allocations: 0, frees: 0, bytes: 0, peak: 0 } }) };
}
struct Allocator;
#[global_allocator]
static ALLOCATOR: Allocator = Allocator;
unsafe impl GlobalAlloc for Allocator {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        let denied = STATE.with(|cell| {
            let mut state = cell.get();
            if !state.enabled {
                return false;
            }
            state.value.attempts += 1;
            let denied = state.deny_all || state.fail_at == state.value.attempts;
            cell.set(state);
            denied
        });
        if denied {
            return std::ptr::null_mut();
        }
        let result = unsafe { System.alloc(layout) };
        if !result.is_null() {
            STATE.with(|cell| {
                let mut state = cell.get();
                if state.enabled {
                    state.value.allocations += 1;
                    state.value.bytes += layout.size() as isize;
                    state.value.peak = state.value.peak.max(state.value.bytes.max(0) as usize);
                    cell.set(state);
                }
            });
        }
        result
    }
    unsafe fn dealloc(&self, pointer: *mut u8, layout: Layout) {
        STATE.with(|cell| {
            let mut state = cell.get();
            if state.enabled {
                state.value.frees += 1;
                state.value.bytes -= layout.size() as isize;
                cell.set(state);
            }
        });
        unsafe {
            System.dealloc(pointer, layout);
        }
    }
}
struct Reset;
impl Drop for Reset {
    fn drop(&mut self) {
        STATE.with(|cell| {
            let mut state = cell.get();
            state.enabled = false;
            cell.set(state);
        });
    }
}
pub(super) fn run<T>(fail_at: usize, deny_all: bool, action: impl FnOnce() -> T) -> (T, Snapshot) {
    STATE.with(|cell| {
        cell.set(State {
            enabled: true,
            fail_at,
            deny_all,
            value: Snapshot::default(),
        })
    });
    let reset = Reset;
    let result = action();
    let snapshot = STATE.with(|cell| cell.get().value);
    drop(reset);
    (result, snapshot)
}
