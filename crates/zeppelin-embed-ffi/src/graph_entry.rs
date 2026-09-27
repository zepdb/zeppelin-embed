//! Graph C entries, exported only with graph-cypher.
use super::{ZeGraphBatchRequest, ZeGraphHandle, ZeGraphOpenRequest, ZeGraphResponse};
use crate::ZeErrorCode;

/// Opens (`mode` 1 read-write, 2 read-only) or creates (`mode` 0) one native
/// graph store; a legacy store directory is refused with
/// `ZE_ERR_STORE_KIND`. `max_resident_bytes` must be in 1..=256 MiB,
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
