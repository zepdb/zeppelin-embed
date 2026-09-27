//! Graph open and close C entries, exported only with graph-cypher.
use super::{ZeGraphHandle, ZeGraphOpenRequest};
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
