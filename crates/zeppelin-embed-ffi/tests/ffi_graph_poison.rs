mod common;
use common::graph::*;
use zeppelin_embed_ffi::*;

/// The ABI panic probe is process-global; tests that arm it must not overlap.
static PROBE_GUARD: std::sync::Mutex<()> = std::sync::Mutex::new(());

fn probe_guard() -> std::sync::MutexGuard<'static, ()> {
    PROBE_GUARD
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

#[test]
fn a_panic_in_a_graph_entry_poisons_only_that_graph_handle() {
    let _guard = probe_guard();
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

#[test]
fn a_panic_after_a_committing_statement_keeps_the_known_outcome() {
    let _guard = probe_guard();
    let mut s = GraphTestStore::create();
    let mut r = empty_response();
    arm_abi_panic_probe("ze_graph_cypher:after-execute");
    assert_eq!(
        ze_graph_cypher(
            s.handle,
            &cypher_request(b"CREATE (:Doc {title:'p'})", &[], None),
            &mut r
        ),
        ZeErrorCode::ZeErrPanic
    );
    assert_eq!(
        (r.disposition, r.changed_generation, r.owner_token),
        (2, 1, 0)
    );
    assert_eq!(s.close(), ZeErrorCode::ZeErrPoisoned);
    let (code, h) = graph_open(&s.path, MODE_READ_WRITE);
    assert_eq!(code, ZeErrorCode::ZeOk);
    let mut r = cypher_ok(h, "MATCH (n:Doc) RETURN n.title AS title");
    assert_eq!(string_of(&r, &rows(&r)[0][0]), "p");
    ze_graph_response_free(&mut r);
    ze_graph_close(h);
}

#[test]
fn declaration_open_panic_has_no_handle_to_poison() {
    let _guard = probe_guard();
    let mut existing = GraphTestStore::create();
    arm_abi_panic_probe("ze_graph_open_with_relationship_types");
    assert_eq!(
        ze_graph_open_with_relationship_types(
            std::ptr::null(),
            std::ptr::null(),
            0,
            std::ptr::null_mut()
        ),
        ZeErrorCode::ZeErrPanic
    );
    assert_eq!(existing.close(), ZeErrorCode::ZeOk);
}

#[test]
fn graph_maintenance_entries_poison_their_graph_handle() {
    let _guard = probe_guard();
    for name in ["ze_graph_maintain", "ze_graph_set_maintenance_policy"] {
        let mut store = GraphTestStore::create();
        let call = || {
            if name == "ze_graph_maintain" {
                let mut report = common::sized_zeroed();
                ze_graph_maintain(store.handle, std::ptr::null(), &mut report)
            } else {
                let policy = ZeGraphMaintenancePolicy {
                    abi_size: 16,
                    automatic: 0,
                    reclaim_after_bytes: 1024 * 1024,
                };
                ze_graph_set_maintenance_policy(store.handle, &policy)
            }
        };
        arm_abi_panic_probe(name);
        assert_eq!(call(), ZeErrorCode::ZeErrPanic, "{name}");
        assert_eq!(call(), ZeErrorCode::ZeErrPoisoned, "{name}");
        assert_eq!(store.close(), ZeErrorCode::ZeErrPoisoned);
    }
}
