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

#[test]
fn ze241_every_graph_handle_export_poisons_its_owner() {
    let _guard = probe_guard();
    for name in [
        "ze_graph_close",
        "ze_graph_apply",
        "ze_graph_cypher",
        "ze_graph_cypher_with_row_limit",
        "ze_graph_query",
        "ze_graph_get_nodes",
        "ze_graph_get_relationships",
        "ze_graph_resources",
        "ze_graph_maintain",
        "ze_graph_set_maintenance_policy",
    ] {
        let mut store = GraphTestStore::create();
        let call = || match name {
            "ze_graph_close" => ze_graph_close(store.handle),
            "ze_graph_apply" => {
                ze_graph_apply(store.handle, std::ptr::null(), std::ptr::null_mut())
            }
            "ze_graph_cypher" => {
                ze_graph_cypher(store.handle, std::ptr::null(), std::ptr::null_mut())
            }
            "ze_graph_cypher_with_row_limit" => ze_graph_cypher_with_row_limit(
                store.handle,
                std::ptr::null(),
                1,
                std::ptr::null_mut(),
            ),
            "ze_graph_query" => {
                ze_graph_query(store.handle, std::ptr::null(), std::ptr::null_mut())
            }
            "ze_graph_get_nodes" => {
                ze_graph_get_nodes(store.handle, std::ptr::null(), std::ptr::null_mut())
            }
            "ze_graph_get_relationships" => {
                ze_graph_get_relationships(store.handle, std::ptr::null(), std::ptr::null_mut())
            }
            "ze_graph_resources" => ze_graph_resources(store.handle, std::ptr::null_mut()),
            "ze_graph_maintain" => {
                ze_graph_maintain(store.handle, std::ptr::null(), std::ptr::null_mut())
            }
            _ => ze_graph_set_maintenance_policy(store.handle, std::ptr::null()),
        };
        arm_abi_panic_probe(name);
        assert_eq!(call(), ZeErrorCode::ZeErrPanic, "{name}");
        assert_eq!(store.close(), ZeErrorCode::ZeErrPoisoned, "{name}");
    }
}

#[test]
fn ze241_query_postcommit_panic_preserves_known_generation() {
    let _guard = probe_guard();
    let mut store = GraphTestStore::create();
    let mut operators: [ZeGraphOperator; 3] = [common::sized_zeroed(); 3];
    operators[1].kind = 5;
    operators[1].inputs.count = 1;
    operators[2].kind = 6;
    operators[2].inputs = ZeGraphRange { start: 1, count: 1 };
    operators[2].mutations.count = 1;
    let mut mutation: ZeGraphMutation = common::sized_zeroed();
    mutation.output = 7;
    let input = [0, 1];
    let pool: ZeGraphValuePool = common::sized_zeroed();
    let mut plan: ZeGraphPlan = common::sized_zeroed();
    plan.root = 2;
    plan.operators = operators.as_ptr();
    plan.operator_count = 3;
    plan.inputs = input.as_ptr();
    plan.input_count = 2;
    plan.mutations = &mutation;
    plan.mutation_count = 1;
    plan.pool = &pool;
    let mut request: ZeGraphQueryRequest = common::sized_zeroed();
    request.plan = &plan;
    let mut response = empty_response();
    arm_abi_panic_probe("ze_graph_query:after-execute");
    assert_eq!(
        ze_graph_query(store.handle, &request, &mut response),
        ZeErrorCode::ZeErrPanic
    );
    assert_eq!(
        (
            response.disposition,
            response.changed_generation,
            response.owner_token
        ),
        (2, 1, 0)
    );
    assert_eq!(store.close(), ZeErrorCode::ZeErrPoisoned);
    let (code, handle) = graph_open(&store.path, MODE_READ_WRITE);
    assert_eq!(code, ZeErrorCode::ZeOk);
    let mut read = cypher_ok(handle, "MATCH (n) RETURN count(n)");
    assert_eq!(rows(&read)[0][0].integer, 1);
    ze_graph_response_free(&mut read);
    ze_graph_close(handle);
}
