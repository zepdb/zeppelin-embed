//! Test-only actions immediately before existing preparation/operator polls.
use super::PlanNodeId;
use std::cell::RefCell;

type Hook = Box<dyn FnMut(bool, PlanNodeId)>;
thread_local! {
    static HOOK: RefCell<Option<Hook>> = const { RefCell::new(None) };
}

pub(super) fn visit(preparing: bool, node: PlanNodeId) {
    HOOK.with(|hook| {
        if let Some(action) = hook.borrow_mut().as_mut() {
            action(preparing, node);
        }
    });
}

pub(super) fn with_hook<R>(
    action: impl FnMut(bool, PlanNodeId) + 'static,
    run: impl FnOnce() -> R,
) -> R {
    struct Reset;
    impl Drop for Reset {
        fn drop(&mut self) {
            HOOK.with(|hook| *hook.borrow_mut() = None);
        }
    }
    HOOK.with(|hook| {
        assert!(hook.borrow().is_none(), "checkpoint hook already installed");
        *hook.borrow_mut() = Some(Box::new(action));
    });
    let _reset = Reset;
    run()
}
