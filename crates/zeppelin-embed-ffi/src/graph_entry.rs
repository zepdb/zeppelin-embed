//! Graph C entries, exported only with graph-cypher.
use super::ZeGraphCypherRequest;
use super::{ZeGraphBatchRequest, ZeGraphHandle, ZeGraphOpenRequest, ZeGraphResponse};
use crate::ZeErrorCode;

/// Opens (`mode` 1 read-write, 2 read-only) or creates (`mode` 0) one native
/// graph store; a legacy store directory is refused with
/// `ZE_ERR_STORE_KIND`; below macOS 14 it is `ZE_ERR_UNSUPPORTED`.
/// `max_resident_bytes` must be in 1..=256 MiB,
/// `tokenizer_profile` must be 0 and `control` must be null.
/// `document_tower` is null for a store without vectors; otherwise node
/// vectors are validated against it and it must match the persisted tower.
#[unsafe(no_mangle)]
pub extern "C" fn ze_graph_open(
    request: *const ZeGraphOpenRequest,
    out_handle: *mut ZeGraphHandle,
) -> ZeErrorCode {
    ffi_entry!(None, ZeErrorCode::ZeErrPanic, {
        crate::finish(None, crate::graph_abi::open(request, out_handle))
    })
}

/// Closes a graph store and releases its handle; outstanding responses stay
/// valid until freed. Closing a stale or closed handle is `ZE_ERR_CLOSED`.
#[unsafe(no_mangle)]
pub extern "C" fn ze_graph_close(handle: ZeGraphHandle) -> ZeErrorCode {
    ffi_entry!(Some(handle.token), ZeErrorCode::ZeErrPanic, {
        crate::run_named_panic_probe("ze_graph_close");
        crate::finish(Some(handle.token), crate::graph_abi::close(handle))
    })
}

/// Applies one atomic structured batch: every node (document) and
/// relationship item commits durably together, or none does. On success
/// `out_response` holds one receipt per item in item order, the disposition
/// and the admitted and changed generations; free it with
/// `ze_graph_response_free`. An exact keyed retry replays.
#[unsafe(no_mangle)]
pub extern "C" fn ze_graph_apply(
    handle: ZeGraphHandle,
    request: *const ZeGraphBatchRequest,
    out_response: *mut ZeGraphResponse,
) -> ZeErrorCode {
    ffi_entry!(Some(handle.token), ZeErrorCode::ZeErrPanic, {
        crate::run_named_panic_probe("ze_graph_apply");
        crate::finish(
            Some(handle.token),
            crate::graph_abi::apply(handle, request, out_response),
        )
    })
}

/// Releases one response and resets it to the empty descriptor. An empty
/// response, including one an error left behind, is accepted; freeing again
/// is a no-op. A forged or altered descriptor is `ZE_ERR_INVALID_ARGUMENT`.
#[unsafe(no_mangle)]
pub extern "C" fn ze_graph_response_free(response: *mut ZeGraphResponse) -> ZeErrorCode {
    ffi_entry!(None, ZeErrorCode::ZeErrPanic, {
        crate::finish(None, crate::graph_abi::free(response))
    })
}

/// Compiles and executes one Cypher statement with bounded nonentity
/// parameters and a default maximum of 1,024 returned rows. Query options
/// declare interpretation and may tighten memory/work limits.
#[unsafe(no_mangle)]
pub extern "C" fn ze_graph_cypher(
    handle: ZeGraphHandle,
    request: *const ZeGraphCypherRequest,
    out_response: *mut ZeGraphResponse,
) -> ZeErrorCode {
    ffi_entry!(Some(handle.token), ZeErrorCode::ZeErrPanic, {
        crate::run_named_panic_probe("ze_graph_cypher");
        crate::finish(
            Some(handle.token),
            crate::graph_abi::cypher(handle, request, out_response),
        )
    })
}

/// Executes Cypher using the frozen request layout and a caller-selected
/// returned-row cap: 0 selects 1,024; 1..=65,536 is accepted. Exceeding the
/// cap fails, never truncates. Other work and memory budgets still apply.
#[unsafe(no_mangle)]
pub extern "C" fn ze_graph_cypher_with_row_limit(
    handle: ZeGraphHandle,
    request: *const ZeGraphCypherRequest,
    result_row_limit: u32,
    out_response: *mut ZeGraphResponse,
) -> ZeErrorCode {
    ffi_entry!(Some(handle.token), ZeErrorCode::ZeErrPanic, {
        crate::run_named_panic_probe("ze_graph_cypher_with_row_limit");
        crate::finish(
            Some(handle.token),
            crate::graph_abi::cypher_with_row_limit(
                handle,
                request,
                result_row_limit,
                out_response,
            ),
        )
    })
}

/// Creates a graph with immutable per-relationship-type incoming-reference rules.
/// `request.mode` must be create (0). Rules survive reopen through ze_graph_open.
/// A child is the source of an edge into the deleted target. Restrict refuses
/// surviving children; cascade deletes them transitively in the same mutation,
/// including Cypher DELETE/DETACH DELETE. Undeclared types keep existing semantics.
/// At most 16384 unique rules and 8 MiB of encoded declarations are accepted.
#[unsafe(no_mangle)]
pub extern "C" fn ze_graph_open_with_relationship_types(
    request: *const ZeGraphOpenRequest,
    rules: *const crate::ZeGraphRelationshipType,
    rule_count: usize,
    out_handle: *mut ZeGraphHandle,
) -> ZeErrorCode {
    ffi_entry!(None, ZeErrorCode::ZeErrPanic, {
        crate::run_named_panic_probe("ze_graph_open_with_relationship_types");
        crate::finish(
            None,
            crate::graph_abi::open_with_relationship_types(request, rules, rule_count, out_handle),
        )
    })
}

/// Sets the per-open writer policy. Read-only handles and thresholds below
/// 1 MiB are refused. A concurrent writer call returns ZE_ERR_BUSY.
#[unsafe(no_mangle)]
pub extern "C" fn ze_graph_set_maintenance_policy(
    handle: ZeGraphHandle,
    policy: *const super::ZeGraphMaintenancePolicy,
) -> ZeErrorCode {
    ffi_entry!(Some(handle.token), ZeErrorCode::ZeErrPanic, {
        crate::run_named_panic_probe("ze_graph_set_maintenance_policy");
        crate::finish(
            Some(handle.token),
            crate::graph_abi::set_maintenance_policy(handle, policy),
        )
    })
}

/// Performs one bounded maintenance step. Loop until cycle_complete is 1
/// to finish a cycle. The caller initializes out_report.abi_size; the report
/// owns no allocations. A concurrent writer call returns ZE_ERR_BUSY.
#[unsafe(no_mangle)]
pub extern "C" fn ze_graph_maintain(
    handle: ZeGraphHandle,
    control: *const super::ZeGraphControl,
    out_report: *mut super::ZeGraphMaintainReport,
) -> ZeErrorCode {
    ffi_entry!(Some(handle.token), ZeErrorCode::ZeErrPanic, {
        crate::run_named_panic_probe("ze_graph_maintain");
        crate::finish(
            Some(handle.token),
            crate::graph_abi::maintain(handle, control, out_report),
        )
    })
}

/// Reads nodes in input order, preserving duplicates and Null for missing IDs.
#[unsafe(no_mangle)]
pub extern "C" fn ze_graph_get_nodes(
    handle: ZeGraphHandle,
    request: *const super::ZeGraphGetNodesRequest,
    out_response: *mut ZeGraphResponse,
) -> ZeErrorCode {
    ffi_entry!(Some(handle.token), ZeErrorCode::ZeErrPanic, {
        crate::run_named_panic_probe("ze_graph_get_nodes");
        crate::finish(
            Some(handle.token),
            crate::graph_abi::get_nodes(handle, request, out_response),
        )
    })
}

/// Reads relationships in input order, preserving duplicates and Null for missing IDs.
#[unsafe(no_mangle)]
pub extern "C" fn ze_graph_get_relationships(
    handle: ZeGraphHandle,
    request: *const super::ZeGraphGetRelsRequest,
    out_response: *mut ZeGraphResponse,
) -> ZeErrorCode {
    ffi_entry!(Some(handle.token), ZeErrorCode::ZeErrPanic, {
        crate::run_named_panic_probe("ze_graph_get_relationships");
        crate::finish(
            Some(handle.token),
            crate::graph_abi::get_relationships(handle, request, out_response),
        )
    })
}

/// Executes one structured native graph plan; no textual query enters core.
/// Output columns are named `slot_<logical ID>`, in validated root-schema order.
/// Names are derived from the core schema; the frozen plan layout is unchanged.
#[unsafe(no_mangle)]
pub extern "C" fn ze_graph_query(
    handle: ZeGraphHandle,
    request: *const super::ZeGraphQueryRequest,
    out_response: *mut ZeGraphResponse,
) -> ZeErrorCode {
    ffi_entry!(Some(handle.token), ZeErrorCode::ZeErrPanic, {
        crate::run_named_panic_probe("ze_graph_query");
        crate::finish(
            Some(handle.token),
            crate::graph_abi::query(handle, request, out_response),
        )
    })
}

/// Returns one coherent allocation snapshot; out must have the exact abi_size
/// and zero abi_reserved. This observes capacities, not process memory or I/O.
#[unsafe(no_mangle)]
pub extern "C" fn ze_graph_resources(
    handle: ZeGraphHandle,
    out: *mut crate::ZeGraphResources,
) -> ZeErrorCode {
    ffi_entry!(Some(handle.token), ZeErrorCode::ZeErrPanic, {
        crate::run_named_panic_probe("ze_graph_resources");
        crate::finish(Some(handle.token), crate::graph_abi::resources(handle, out))
    })
}
