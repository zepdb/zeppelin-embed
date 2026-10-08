//! Graph C entries on the unified Store handle.
use super::{ZeGraphBatchRequest, ZeGraphCypherRequest, ZeGraphOpenRequest, ZeGraphResponse};
use crate::{ZeErrorCode, ZeHandle};

// The graph-free ABI refuses at the boundary, without compiling the engine.
macro_rules! graph_call {
    ($method:ident($($arg:expr),* $(,)?)) => {{
        #[cfg(feature = "graph-cypher")]
        { crate::graph_abi::$method($($arg),*) }
        #[cfg(not(feature = "graph-cypher"))]
        {
            let _ = ($($arg),*);
            unsupported_graph()
        }
    }};
}

#[cfg(not(feature = "graph-cypher"))]
#[inline(never)]
fn unsupported_graph() -> Result<(), crate::error::FfiError> {
    Err(crate::error::FfiError::new(
        ZeErrorCode::ZeErrGraphUnsupportedBuild,
        "this build does not support graph operations",
    ))
}

/// Applies one atomic structured batch: every node (document) and
/// relationship item commits durably together, or none does. On success
/// `out_response` holds one receipt per item in item order, the disposition
/// and the admitted and changed generations; free it with
/// `ze_graph_response_free`. An exact keyed retry replays.
#[unsafe(no_mangle)]
pub extern "C" fn ze_store_graph_apply(
    handle: ZeHandle,
    request: *const ZeGraphBatchRequest,
    out_response: *mut ZeGraphResponse,
) -> ZeErrorCode {
    ffi_entry!(Some(handle), ZeErrorCode::ZeErrPanic, {
        crate::run_named_panic_probe("ze_store_graph_apply");
        crate::finish(
            Some(handle),
            graph_call!(apply(handle, request, out_response)),
        )
    })
}

/// Releases one response and resets it to the empty descriptor. An empty
/// response, including one an error left behind, is accepted; freeing again
/// is a no-op. A forged or altered descriptor is `ZE_ERR_INVALID_ARGUMENT`.
#[unsafe(no_mangle)]
pub extern "C" fn ze_graph_response_free(response: *mut ZeGraphResponse) -> ZeErrorCode {
    ffi_entry!(None, ZeErrorCode::ZeErrPanic, {
        crate::finish(None, graph_call!(free(response)))
    })
}

/// Compiles and executes one Cypher statement with bounded nonentity
/// parameters and a default maximum of 1,024 returned rows. Query options
/// declare interpretation and may tighten memory/work limits.
#[unsafe(no_mangle)]
pub extern "C" fn ze_store_cypher(
    handle: ZeHandle,
    request: *const ZeGraphCypherRequest,
    out_response: *mut ZeGraphResponse,
) -> ZeErrorCode {
    ffi_entry!(Some(handle), ZeErrorCode::ZeErrPanic, {
        crate::run_named_panic_probe("ze_store_cypher");
        crate::finish(
            Some(handle),
            graph_call!(cypher(handle, request, out_response)),
        )
    })
}

/// Executes Cypher using the frozen request layout and a caller-selected
/// returned-row cap: 0 selects 1,024; 1..=65,536 is accepted. Exceeding the
/// cap fails, never truncates. Other work and memory budgets still apply.
#[unsafe(no_mangle)]
pub extern "C" fn ze_store_cypher_with_row_limit(
    handle: ZeHandle,
    request: *const ZeGraphCypherRequest,
    result_row_limit: u32,
    out_response: *mut ZeGraphResponse,
) -> ZeErrorCode {
    ffi_entry!(Some(handle), ZeErrorCode::ZeErrPanic, {
        crate::run_named_panic_probe("ze_store_cypher_with_row_limit");
        crate::finish(
            Some(handle),
            graph_call!(cypher_with_row_limit(
                handle,
                request,
                result_row_limit,
                out_response,
            )),
        )
    })
}

/// Creates a Store with immutable per-relationship-type incoming-reference rules.
/// `request.mode` must be create (0). Rules survive reopen through ze_open.
/// A child is the source of an edge into the deleted target. Restrict refuses
/// surviving children; cascade deletes them transitively in the same mutation,
/// including Cypher DELETE/DETACH DELETE. Undeclared types keep existing semantics.
/// At most 16384 unique rules and 8 MiB of encoded declarations are accepted.
#[unsafe(no_mangle)]
pub extern "C" fn ze_store_create_with_relationship_types(
    request: *const ZeGraphOpenRequest,
    rules: *const crate::ZeGraphRelationshipType,
    rule_count: usize,
    out_handle: *mut ZeHandle,
) -> ZeErrorCode {
    ffi_entry!(None, ZeErrorCode::ZeErrPanic, {
        crate::run_named_panic_probe("ze_store_create_with_relationship_types");
        crate::finish(
            None,
            graph_call!(open_with_relationship_types(
                request, rules, rule_count, out_handle
            )),
        )
    })
}

/// Sets the per-open writer policy. Read-only handles and thresholds below
/// 1 MiB are refused. A concurrent writer call returns ZE_ERR_BUSY.
#[unsafe(no_mangle)]
pub extern "C" fn ze_store_set_graph_maintenance_policy(
    handle: ZeHandle,
    policy: *const super::ZeGraphMaintenancePolicy,
) -> ZeErrorCode {
    ffi_entry!(Some(handle), ZeErrorCode::ZeErrPanic, {
        crate::run_named_panic_probe("ze_store_set_graph_maintenance_policy");
        crate::finish(
            Some(handle),
            graph_call!(set_maintenance_policy(handle, policy)),
        )
    })
}

/// Performs one bounded maintenance step. Loop until cycle_complete is 1
/// to finish a cycle. The caller initializes out_report.abi_size; the report
/// owns no allocations. A concurrent writer call returns ZE_ERR_BUSY.
#[unsafe(no_mangle)]
pub extern "C" fn ze_store_graph_maintain(
    handle: ZeHandle,
    control: *const super::ZeGraphControl,
    out_report: *mut super::ZeGraphMaintainReport,
) -> ZeErrorCode {
    ffi_entry!(Some(handle), ZeErrorCode::ZeErrPanic, {
        crate::run_named_panic_probe("ze_store_graph_maintain");
        crate::finish(
            Some(handle),
            graph_call!(maintain(handle, control, out_report)),
        )
    })
}

/// Reads nodes in input order, preserving duplicates and Null for missing IDs.
#[unsafe(no_mangle)]
pub extern "C" fn ze_store_get_nodes(
    handle: ZeHandle,
    request: *const super::ZeGraphGetNodesRequest,
    out_response: *mut ZeGraphResponse,
) -> ZeErrorCode {
    ffi_entry!(Some(handle), ZeErrorCode::ZeErrPanic, {
        crate::run_named_panic_probe("ze_store_get_nodes");
        crate::finish(
            Some(handle),
            graph_call!(get_nodes(handle, request, out_response)),
        )
    })
}

/// Reads relationships in input order, preserving duplicates and Null for missing IDs.
#[unsafe(no_mangle)]
pub extern "C" fn ze_store_get_relationships(
    handle: ZeHandle,
    request: *const super::ZeGraphGetRelsRequest,
    out_response: *mut ZeGraphResponse,
) -> ZeErrorCode {
    ffi_entry!(Some(handle), ZeErrorCode::ZeErrPanic, {
        crate::run_named_panic_probe("ze_store_get_relationships");
        crate::finish(
            Some(handle),
            graph_call!(get_relationships(handle, request, out_response)),
        )
    })
}

/// Executes one structured native graph plan; no textual query enters core.
/// Output columns are named `slot_<logical ID>`, in validated root-schema order.
/// Names are derived from the core schema; the frozen plan layout is unchanged.
#[unsafe(no_mangle)]
pub extern "C" fn ze_store_graph_query(
    handle: ZeHandle,
    request: *const super::ZeGraphQueryRequest,
    out_response: *mut ZeGraphResponse,
) -> ZeErrorCode {
    ffi_entry!(Some(handle), ZeErrorCode::ZeErrPanic, {
        crate::run_named_panic_probe("ze_store_graph_query");
        crate::finish(
            Some(handle),
            graph_call!(query(handle, request, out_response)),
        )
    })
}

/// Returns one coherent allocation snapshot; out must have the exact abi_size
/// and zero abi_reserved. This observes capacities, not process memory or I/O.
#[unsafe(no_mangle)]
pub extern "C" fn ze_store_graph_resources(
    handle: ZeHandle,
    out: *mut crate::ZeGraphResources,
) -> ZeErrorCode {
    ffi_entry!(Some(handle), ZeErrorCode::ZeErrPanic, {
        crate::run_named_panic_probe("ze_store_graph_resources");
        crate::finish(Some(handle), graph_call!(resources(handle, out)))
    })
}

/// Enables graph storage using the Store writer and returns its generation.
/// Repeated calls are idempotent; read-only stores refuse.
#[unsafe(no_mangle)]
pub extern "C" fn ze_store_enable_graph(
    handle: ZeHandle,
    out_report: *mut crate::ZeGenerationReport,
) -> ZeErrorCode {
    ffi_entry!(Some(handle), ZeErrorCode::ZeErrPanic, {
        crate::run_named_panic_probe("ze_store_enable_graph");
        crate::finish(Some(handle), graph_call!(enable_graph(handle, out_report)))
    })
}
