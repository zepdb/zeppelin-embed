mod common;
use common::graph::*;
use zeppelin_embed_ffi::*;

#[test]
fn a_panic_in_a_graph_entry_poisons_only_that_graph_handle() {
    let mut first = GraphTestStore::create();
    let mut second = GraphTestStore::create();
    let legacy = common::TestStore::new();
    arm_abi_panic_probe("ze_graph_close");
    assert_eq!(ze_graph_close(first.handle), ZeErrorCode::ZeErrPanic);
    assert_eq!(
        last_error(first.handle.token),
        "abi panic probe in ze_graph_close"
    );
    let poisoned = first.handle;
    assert_eq!(first.close(), ZeErrorCode::ZeErrPoisoned);
    assert_eq!(ze_graph_close(poisoned), ZeErrorCode::ZeErrClosed);
    assert_eq!(second.close(), ZeErrorCode::ZeOk);
    let mut state = common::sized_zeroed();
    assert_eq!(ze_state(legacy.handle, &mut state), ZeErrorCode::ZeOk);
}
