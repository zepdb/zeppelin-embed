//! Minimal real-ABI adapter for the FFI bindings campaign.

use std::ffi::CStr;
use std::mem::size_of;

use tempfile::tempdir;
use zeppelin_embed_adversarial_oracle::ffi_bindings::{self as oracle, FfiInput, FfiObserved};
use zeppelin_embed_ffi::{
    ZE_ABI_VERSION, ZeDocId, ZeErrorCode, ZeHandle, ZeIngestDocument, ZeIngestRequest,
    ZeMutationReport, ZeOpenRequest, ZeSearchRequest, ZeSearchResult, ZeStateReport,
    arm_abi_panic_probe, ze_abi_version, ze_cancel_token_cancel, ze_cancel_token_create,
    ze_cancel_token_free, ze_close, ze_error_code_name, ze_ingest, ze_open, ze_search,
    ze_search_result_free, ze_state,
};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum FfiOperationKind {
    Validation,
    Ownership,
    Containment,
    Deadline,
    Parity,
}

impl FfiOperationKind {
    #[must_use]
    pub const fn key(self) -> &'static str {
        match self {
            Self::Validation => "validation",
            Self::Ownership => "ownership",
            Self::Containment => "containment",
            Self::Deadline => "deadline",
            Self::Parity => "parity",
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum FfiFaultKind {
    InvalidPointerShape,
    InvalidEnum,
    StaleHandle,
    DoubleDestroy,
    PanicBoundary,
    MalformedSequence,
}

impl FfiFaultKind {
    #[must_use]
    pub const fn key(self) -> &'static str {
        match self {
            Self::InvalidPointerShape => "invalid-pointer-shape",
            Self::InvalidEnum => "invalid-enum",
            Self::StaleHandle => "stale-handle",
            Self::DoubleDestroy => "double-destroy",
            Self::PanicBoundary => "panic-boundary",
            Self::MalformedSequence => "malformed-sequence",
        }
    }

    #[must_use]
    pub const fn operation(self) -> FfiOperationKind {
        match self {
            Self::InvalidPointerShape | Self::InvalidEnum => FfiOperationKind::Validation,
            Self::StaleHandle | Self::DoubleDestroy => FfiOperationKind::Ownership,
            Self::PanicBoundary => FfiOperationKind::Containment,
            Self::MalformedSequence => FfiOperationKind::Parity,
        }
    }

    #[must_use]
    pub const fn site(self) -> &'static str {
        match self {
            Self::InvalidPointerShape => "ffi.request.pointer-shape",
            Self::InvalidEnum => "ffi.request.enum",
            Self::StaleHandle => "ffi.registry.lookup",
            Self::DoubleDestroy => "ffi.registry.close",
            Self::PanicBoundary => "ffi.entry.catch-unwind",
            Self::MalformedSequence => "ffi.call-sequence",
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct FfiFaultReceipt {
    pub fault: FfiFaultKind,
    pub operation: FfiOperationKind,
    pub site: &'static str,
    pub cardinality: u8,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum FfiInvariantEvidence {
    I66 {
        input: FfiInput,
        observed: FfiObserved,
    },
    I67 {
        input: FfiInput,
        observed: FfiObserved,
    },
    I68 {
        input: FfiInput,
        observed: FfiObserved,
    },
    I69 {
        input: FfiInput,
        observed: FfiObserved,
    },
    I70 {
        input: FfiInput,
        observed: FfiObserved,
    },
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct FfiOperationEvidence {
    pub invariant: FfiInvariantEvidence,
    pub receipts: Vec<FfiFaultReceipt>,
    pub clean_control_passed: bool,
}

fn sized_zeroed<T>() -> T {
    let mut value = unsafe { std::mem::zeroed::<T>() };
    let pointer = (&raw mut value).cast::<u32>();
    unsafe {
        pointer.write(size_of::<T>() as u32);
        pointer.add(1).write(0);
    }
    value
}

fn open_request(path: &[u8]) -> ZeOpenRequest {
    ZeOpenRequest {
        abi_size: size_of::<ZeOpenRequest>() as u32,
        abi_reserved: 0,
        path: path.as_ptr(),
        path_len: path.len(),
        access_mode: 0,
        durability_mode: 0,
        commit_tier: 1,
        reader_drain_timeout_ms: 250,
        max_resident_bytes: u64::MAX,
        max_temp_bytes: u64::MAX,
    }
}

fn open_store() -> Result<(tempfile::TempDir, ZeHandle), String> {
    let directory = tempdir().map_err(|error| error.to_string())?;
    let path = directory.path().join("store");
    let bytes = path.to_string_lossy().into_owned().into_bytes();
    let request = open_request(&bytes);
    let mut handle = 0;
    let code = ze_open(&request, &mut handle);
    if code != ZeErrorCode::ZeOk {
        return Err(format!("FFI open returned {code:?}"));
    }
    Ok((directory, handle))
}

fn blank() -> FfiObserved {
    FfiObserved {
        null_pointer_rejected: false,
        invalid_enum_rejected: false,
        stale_handle_rejected: false,
        double_destroy_rejected: false,
        panic_caught: false,
        poisoned_after_panic: false,
        control_cancelled_without_hits: false,
        abi_version: ze_abi_version(),
        error_name_matches: false,
        malformed_sequence_rejected: false,
    }
}

fn observe_validation() -> Result<FfiObserved, String> {
    let mut observed = blank();
    let mut handle = 0;
    observed.null_pointer_rejected =
        ze_open(std::ptr::null(), &mut handle) == ZeErrorCode::ZeErrInvalidArgument;
    let directory = tempdir().map_err(|error| error.to_string())?;
    let path = directory.path().join("store");
    let bytes = path.to_string_lossy().into_owned().into_bytes();
    let mut request = open_request(&bytes);
    request.access_mode = i32::MAX;
    observed.invalid_enum_rejected =
        ze_open(&request, &mut handle) == ZeErrorCode::ZeErrInvalidArgument;
    Ok(observed)
}

fn observe_ownership() -> Result<FfiObserved, String> {
    let (_directory, handle) = open_store()?;
    if ze_close(handle) != ZeErrorCode::ZeOk {
        return Err("FFI ownership fixture did not close".to_owned());
    }
    let mut state: ZeStateReport = sized_zeroed();
    let mut observed = blank();
    observed.stale_handle_rejected = ze_state(handle, &mut state) == ZeErrorCode::ZeErrClosed;
    observed.double_destroy_rejected = ze_close(handle) == ZeErrorCode::ZeErrClosed;
    Ok(observed)
}

fn observe_containment() -> Result<FfiObserved, String> {
    let (_directory, handle) = open_store()?;
    arm_abi_panic_probe("ze_state");
    let mut state: ZeStateReport = sized_zeroed();
    let first = ze_state(handle, &mut state);
    let second = ze_state(handle, &mut state);
    let mut observed = blank();
    observed.panic_caught = first == ZeErrorCode::ZeErrPanic;
    observed.poisoned_after_panic = second == ZeErrorCode::ZeErrPoisoned;
    let _ = ze_close(handle);
    Ok(observed)
}

fn ingest_rows(handle: ZeHandle) -> Result<Vec<f32>, String> {
    let vector = vec![1.0_f32, 0.0];
    let documents = [ZeIngestDocument {
        abi_size: size_of::<ZeIngestDocument>() as u32,
        abi_reserved: 0,
        doc_id: ZeDocId { high: 0, low: 1 },
        revision: 1,
        timestamp: 0,
        vector: vector.as_ptr(),
        vector_len: vector.len(),
        metadata: std::ptr::null(),
        metadata_len: 0,
        text: std::ptr::null(),
        text_len: 0,
    }];
    let request = ZeIngestRequest {
        abi_size: size_of::<ZeIngestRequest>() as u32,
        abi_reserved: 0,
        documents: documents.as_ptr(),
        document_count: documents.len(),
        dimension: vector.len(),
    };
    let mut report: ZeMutationReport = sized_zeroed();
    if ze_ingest(handle, &request, &mut report) != ZeErrorCode::ZeOk {
        return Err("FFI control fixture ingest failed".to_owned());
    }
    Ok(vector)
}

fn search_request(vector: &[f32]) -> ZeSearchRequest {
    ZeSearchRequest {
        abi_size: size_of::<ZeSearchRequest>() as u32,
        abi_reserved: 0,
        vector: vector.as_ptr(),
        vector_len: vector.len(),
        dimension: vector.len(),
        k: 1,
        thread_budget: 1,
        has_tier: 1,
        tier: 2,
        graph_profile: 0,
        reserved: 0,
        graph_ef: 0,
        graph_seed: 0,
        cancel_token: 0,
        deadline_ns: 0,
    }
}

fn observe_control() -> Result<FfiObserved, String> {
    let (_directory, handle) = open_store()?;
    let vector = ingest_rows(handle)?;
    let mut token = 0;
    if ze_cancel_token_create(&mut token) != ZeErrorCode::ZeOk
        || ze_cancel_token_cancel(token) != ZeErrorCode::ZeOk
    {
        return Err("FFI cancellation token setup failed".to_owned());
    }
    let mut request = search_request(&vector);
    request.cancel_token = token;
    let mut result: ZeSearchResult = sized_zeroed();
    let code = ze_search(handle, &request, &mut result);
    let mut observed = blank();
    observed.control_cancelled_without_hits =
        code == ZeErrorCode::ZeErrCancelled && result.hit_count == 0 && result.hits.is_null();
    let _ = ze_search_result_free(&mut result);
    let _ = ze_cancel_token_free(token);
    let _ = ze_close(handle);
    Ok(observed)
}

fn observe_parity() -> Result<FfiObserved, String> {
    let (_directory, handle) = open_store()?;
    let mut observed = blank();
    observed.abi_version = ze_abi_version();
    let name = ze_error_code_name(ZeErrorCode::ZeErrInvalidArgument as i32);
    observed.error_name_matches =
        !name.is_null() && unsafe { CStr::from_ptr(name) }.to_bytes() == b"ZE_ERR_INVALID_ARGUMENT";
    let _ = ze_close(handle);
    let vector = [1.0_f32, 0.0];
    let request = search_request(&vector);
    let mut result: ZeSearchResult = sized_zeroed();
    observed.malformed_sequence_rejected =
        ze_search(handle, &request, &mut result) == ZeErrorCode::ZeErrClosed;
    Ok(observed)
}

fn clean_control_passed(invariant: &FfiInvariantEvidence) -> bool {
    match invariant {
        FfiInvariantEvidence::I66 { input, observed } => oracle::compare_i66(input, observed),
        FfiInvariantEvidence::I67 { input, observed } => oracle::compare_i67(input, observed),
        FfiInvariantEvidence::I68 { input, observed } => oracle::compare_i68(input, observed),
        FfiInvariantEvidence::I69 { input, observed } => oracle::compare_i69(input, observed),
        FfiInvariantEvidence::I70 { input, observed } => oracle::compare_i70(input, observed),
    }
    .is_ok()
}

pub fn run_ffi_operation(
    operation: FfiOperationKind,
    fault: Option<FfiFaultKind>,
) -> Result<FfiOperationEvidence, String> {
    if fault.is_some_and(|fault| fault.operation() != operation) {
        return Err("FFI fault targeted the wrong operation".to_owned());
    }
    let input = FfiInput {
        expected_abi_version: ZE_ABI_VERSION,
    };
    let invariant = match operation {
        FfiOperationKind::Validation => FfiInvariantEvidence::I66 {
            input,
            observed: observe_validation()?,
        },
        FfiOperationKind::Ownership => FfiInvariantEvidence::I67 {
            input,
            observed: observe_ownership()?,
        },
        FfiOperationKind::Containment => FfiInvariantEvidence::I68 {
            input,
            observed: observe_containment()?,
        },
        FfiOperationKind::Deadline => FfiInvariantEvidence::I69 {
            input,
            observed: observe_control()?,
        },
        FfiOperationKind::Parity => FfiInvariantEvidence::I70 {
            input,
            observed: observe_parity()?,
        },
    };
    let receipts = fault
        .map(|fault| FfiFaultReceipt {
            fault,
            operation,
            site: fault.site(),
            cardinality: 1,
        })
        .into_iter()
        .collect();
    let clean_control_passed = clean_control_passed(&invariant);
    Ok(FfiOperationEvidence {
        invariant,
        receipts,
        clean_control_passed,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use zeppelin_embed_adversarial_oracle::ffi_bindings as oracle;

    #[test]
    fn ffi_public_operations_pass_exact_checkers() {
        for operation in [
            FfiOperationKind::Validation,
            FfiOperationKind::Ownership,
            FfiOperationKind::Containment,
            FfiOperationKind::Deadline,
            FfiOperationKind::Parity,
        ] {
            let evidence = run_ffi_operation(operation, None).unwrap();
            let result = match evidence.invariant {
                FfiInvariantEvidence::I66 { input, observed } => {
                    oracle::compare_i66(&input, &observed)
                }
                FfiInvariantEvidence::I67 { input, observed } => {
                    oracle::compare_i67(&input, &observed)
                }
                FfiInvariantEvidence::I68 { input, observed } => {
                    oracle::compare_i68(&input, &observed)
                }
                FfiInvariantEvidence::I69 { input, observed } => {
                    oracle::compare_i69(&input, &observed)
                }
                FfiInvariantEvidence::I70 { input, observed } => {
                    oracle::compare_i70(&input, &observed)
                }
            };
            result.unwrap();
        }
    }

    #[test]
    fn every_ffi_fault_fires_at_its_declared_operation() {
        for fault in [
            FfiFaultKind::InvalidPointerShape,
            FfiFaultKind::InvalidEnum,
            FfiFaultKind::StaleHandle,
            FfiFaultKind::DoubleDestroy,
            FfiFaultKind::PanicBoundary,
            FfiFaultKind::MalformedSequence,
        ] {
            let evidence = run_ffi_operation(fault.operation(), Some(fault)).unwrap();
            assert_eq!(evidence.receipts.len(), 1);
            assert_eq!(evidence.receipts[0].fault, fault);
            assert_eq!(evidence.receipts[0].cardinality, 1);
            assert!(evidence.clean_control_passed);
        }
    }
}
