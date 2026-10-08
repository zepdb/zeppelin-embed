//! Nonshipping bridge: install faults on the thread entering the real export.
//! Modes: 0 clean, 1 allocation refusal, 2 preappend refusal, 3 sync uncertainty,
//! 4 postcommit cancellation, 5 uncertain panic, 6 known-commit panic.
//! Receipts count actual fires.
use crate::*;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use zeppelin_embed::vfs::file_test_support::{FileEvent, FileOperationScope};

/// Run a real C call with a calling-thread fault scope. Never shipped.
///
/// # Safety
/// `request`, `response` and `fires` must name live honest caller storage.
/// cbindgen:ignore
#[unsafe(no_mangle)]
#[allow(clippy::panic)]
pub unsafe extern "C" fn ze72_test_cypher(
    handle: ZeHandle,
    request: *const ZeGraphCypherRequest,
    response: *mut ZeGraphResponse,
    mode: u32,
    fires: *mut u64,
) -> ZeErrorCode {
    if request.is_null() || response.is_null() || fires.is_null() || mode > 6 {
        return ZeErrorCode::ZeErrInvalidArgument;
    }
    let count = Arc::new(AtomicU64::new(0));
    let hook_count = count.clone();
    let allocation = graph_result::test_support::AllocationFaultScope::arm(usize::from(mode == 1));
    let mut cancel = 0;
    if mode == 4 {
        let code = ze_cancel_token_create(&mut cancel);
        if code != ZeErrorCode::ZeOk {
            return code;
        }
    }
    let mut copied = unsafe { *request };
    let control = ZeGraphControl {
        abi_size: std::mem::size_of::<ZeGraphControl>() as u32,
        abi_reserved: 0,
        cancel_token: cancel,
        deadline_ns: 0,
    };
    if mode == 4 {
        copied.control = &control;
    }
    let mut append_seen = false;
    let scope = FileOperationScope::install(move |event| {
        if event == FileEvent::BeforeAppend {
            append_seen = true;
        }
        let matches = match mode {
            2 => event == FileEvent::BeforeAppend,
            3 => append_seen && event == FileEvent::BeforeSync,
            4 | 5 => append_seen && event == FileEvent::AfterSync,
            _ => false,
        };
        if matches && hook_count.fetch_add(1, Ordering::SeqCst) == 0 {
            match mode {
                2 | 3 => return Err(std::io::Error::other("ZE-72 directed binding I/O refusal")),
                4 => {
                    let _ = ze_cancel_token_cancel(cancel);
                }
                5 => panic!("ZE-72 actual postcommit panic boundary"),
                _ => {}
            }
        }
        Ok(())
    });
    if mode == 6 {
        arm_abi_panic_probe("ze_store_cypher:after-execute");
    }
    let panic_fires = abi_panic_probe_fire_count();
    let code = ze_store_cypher(handle, &copied, response);
    count.fetch_add(abi_panic_probe_fire_count() - panic_fires, Ordering::SeqCst);
    drop(scope);
    unsafe {
        *fires = count.load(Ordering::SeqCst) + allocation.receipt().fires as u64;
    }
    drop(allocation);
    if cancel != 0 {
        let _ = ze_cancel_token_free(cancel);
    }
    code
}

/// Pause the real apply call at its first WAL append boundary for binding
/// reentrancy/close tests. The callback runs on the actual blocking-call thread.
///
/// # Safety
/// All pointers and callback context must remain live through the call. The
/// callback must return normally and must not reenter this native write.
/// cbindgen:ignore
#[unsafe(no_mangle)]
pub unsafe extern "C" fn ze72_test_apply_at_append(
    handle: ZeHandle,
    request: *const ZeGraphBatchRequest,
    response: *mut ZeGraphResponse,
    callback: Option<extern "C" fn(*mut std::ffi::c_void)>,
    context: *mut std::ffi::c_void,
    fires: *mut u64,
) -> ZeErrorCode {
    if request.is_null() || response.is_null() || fires.is_null() {
        return ZeErrorCode::ZeErrInvalidArgument;
    }
    let Some(callback) = callback else {
        return ZeErrorCode::ZeErrInvalidArgument;
    };
    let count = Arc::new(AtomicU64::new(0));
    let receipt = count.clone();
    let scope = FileOperationScope::install(move |event| {
        if event == FileEvent::BeforeAppend && count.fetch_add(1, Ordering::SeqCst) == 0 {
            callback(context);
        }
        Ok(())
    });
    let code = ze_store_graph_apply(handle, request, response);
    drop(scope);
    unsafe {
        *fires = receipt.load(Ordering::SeqCst);
    }
    code
}
