mod common;

use std::mem::size_of;

use zeppelin_embed_ffi::*;

#[derive(Clone, Copy, Debug)]
enum Cell {
    NullPointer,
    WrongDimensions,
    ZeroK,
    HugeK,
    MisalignedBuffer,
    InvalidUtf8Path,
    InteriorNulPath,
    BadEnumDiscriminant,
    UndersizedAbi,
    OversizedAbi,
    CountLengthOverflow,
}

const CELLS: &[Cell] = &[
    Cell::NullPointer,
    Cell::WrongDimensions,
    Cell::ZeroK,
    Cell::HugeK,
    Cell::MisalignedBuffer,
    Cell::InvalidUtf8Path,
    Cell::InteriorNulPath,
    Cell::BadEnumDiscriminant,
    Cell::UndersizedAbi,
    Cell::OversizedAbi,
    Cell::CountLengthOverflow,
];

enum CellResult {
    Executed(ZeErrorCode),
    Skipped(&'static str),
}

struct MatrixContext {
    store: common::TestStore,
    vector: Vec<f32>,
    ids: Vec<ZeDocId>,
}

type MatrixCall = fn(&MatrixContext, Cell) -> CellResult;

#[derive(Clone, Copy)]
enum AbiCoverage {
    DetailedMatrix,
    InvalidProbe(fn(&MatrixContext) -> ProbeResult),
    ValueProbe(fn() -> bool),
}

enum ProbeResult {
    Status(ZeErrorCode),
    Pointer(*const std::ffi::c_char),
}

struct AbiEntry {
    name: &'static str,
    coverage: AbiCoverage,
}

const DETAILED_MATRIX: &[(&str, MatrixCall)] = &[
    ("ze_open", open_cell),
    ("ze_open_with_epoch", open_with_epoch_cell),
    ("ze_epoch_identity", epoch_identity_cell),
    ("ze_epoch_switch_alias", epoch_alias_cell),
    ("ze_epoch_drop", epoch_drop_cell),
    ("ze_ingest", ingest_cell),
    ("ze_delete", delete_cell),
    ("ze_search", search_cell),
    ("ze_query", query_cell),
    ("ze_seal", seal_cell),
    ("ze_drop_partition", drop_cell),
    ("ze_apply_retention", retention_cell),
    ("ze_purge", purge_cell),
    ("ze_await_physical_purge", await_cell),
    ("ze_maintain", maintain_cell),
];

const ABI_REGISTRY: &[AbiEntry] = &[
    AbiEntry {
        name: "ze_abi_version",
        coverage: AbiCoverage::ValueProbe(probe_abi_version),
    },
    AbiEntry {
        name: "ze_open",
        coverage: AbiCoverage::DetailedMatrix,
    },
    AbiEntry {
        name: "ze_open_with_epoch",
        coverage: AbiCoverage::DetailedMatrix,
    },
    AbiEntry {
        name: "ze_namespace_open",
        coverage: AbiCoverage::InvalidProbe(probe_namespace_open),
    },
    AbiEntry {
        name: "ze_namespace_list",
        coverage: AbiCoverage::InvalidProbe(probe_namespace_list),
    },
    AbiEntry {
        name: "ze_namespace_list_result_free",
        coverage: AbiCoverage::InvalidProbe(probe_namespace_list_result_free),
    },
    AbiEntry {
        name: "ze_epoch_identity",
        coverage: AbiCoverage::DetailedMatrix,
    },
    AbiEntry {
        name: "ze_epoch_current",
        coverage: AbiCoverage::InvalidProbe(probe_epoch_current),
    },
    AbiEntry {
        name: "ze_epoch_switch_alias",
        coverage: AbiCoverage::DetailedMatrix,
    },
    AbiEntry {
        name: "ze_epoch_drop",
        coverage: AbiCoverage::DetailedMatrix,
    },
    AbiEntry {
        name: "ze_close",
        coverage: AbiCoverage::InvalidProbe(probe_close),
    },
    AbiEntry {
        name: "ze_state",
        coverage: AbiCoverage::InvalidProbe(probe_state),
    },
    AbiEntry {
        name: "ze_stats",
        coverage: AbiCoverage::InvalidProbe(probe_stats),
    },
    AbiEntry {
        name: "ze_ingest",
        coverage: AbiCoverage::DetailedMatrix,
    },
    AbiEntry {
        name: "ze_upsert",
        coverage: AbiCoverage::InvalidProbe(probe_upsert),
    },
    AbiEntry {
        name: "ze_delete",
        coverage: AbiCoverage::DetailedMatrix,
    },
    AbiEntry {
        name: "ze_search",
        coverage: AbiCoverage::DetailedMatrix,
    },
    AbiEntry {
        name: "ze_query",
        coverage: AbiCoverage::DetailedMatrix,
    },
    AbiEntry {
        name: "ze_query_result_free",
        coverage: AbiCoverage::InvalidProbe(probe_query_result_free),
    },
    AbiEntry {
        name: "ze_seal",
        coverage: AbiCoverage::DetailedMatrix,
    },
    AbiEntry {
        name: "ze_drop_partition",
        coverage: AbiCoverage::DetailedMatrix,
    },
    AbiEntry {
        name: "ze_apply_retention",
        coverage: AbiCoverage::DetailedMatrix,
    },
    AbiEntry {
        name: "ze_purge",
        coverage: AbiCoverage::DetailedMatrix,
    },
    AbiEntry {
        name: "ze_await_physical_purge",
        coverage: AbiCoverage::DetailedMatrix,
    },
    AbiEntry {
        name: "ze_maintain",
        coverage: AbiCoverage::DetailedMatrix,
    },
    AbiEntry {
        name: "ze_last_error_message",
        coverage: AbiCoverage::InvalidProbe(probe_last_error_message),
    },
    AbiEntry {
        name: "ze_error_code_name",
        coverage: AbiCoverage::InvalidProbe(probe_error_code_name),
    },
    AbiEntry {
        name: "ze_cancel_token_create",
        coverage: AbiCoverage::InvalidProbe(probe_cancel_create),
    },
    AbiEntry {
        name: "ze_cancel_token_cancel",
        coverage: AbiCoverage::InvalidProbe(probe_cancel_cancel),
    },
    AbiEntry {
        name: "ze_cancel_token_free",
        coverage: AbiCoverage::InvalidProbe(probe_cancel_free),
    },
    AbiEntry {
        name: "ze_search_result_free",
        coverage: AbiCoverage::InvalidProbe(probe_search_result_free),
    },
    #[cfg(feature = "text")]
    AbiEntry {
        name: "ze_text_open",
        coverage: AbiCoverage::InvalidProbe(probe_text_open),
    },
    #[cfg(feature = "text")]
    AbiEntry {
        name: "ze_text_ingest",
        coverage: AbiCoverage::InvalidProbe(probe_text_ingest),
    },
    #[cfg(feature = "text")]
    AbiEntry {
        name: "ze_text_maintain",
        coverage: AbiCoverage::InvalidProbe(probe_text_maintain),
    },
    #[cfg(feature = "text")]
    AbiEntry {
        name: "ze_text_query",
        coverage: AbiCoverage::InvalidProbe(probe_text_query),
    },
    #[cfg(feature = "text")]
    AbiEntry {
        name: "ze_text_query_result_free",
        coverage: AbiCoverage::InvalidProbe(probe_text_query_result_free),
    },
];

fn expected_error(code: ZeErrorCode) -> CellResult {
    CellResult::Executed(code)
}

fn not_applicable(reason: &'static str) -> CellResult {
    CellResult::Skipped(reason)
}

fn open_cell(context: &MatrixContext, cell: Cell) -> CellResult {
    let valid_path = context.path_bytes();
    let mut request = ZeOpenRequest {
        abi_size: size_of::<ZeOpenRequest>() as u32,
        abi_reserved: 0,
        path: valid_path.as_ptr(),
        path_len: valid_path.len(),
        access_mode: 1,
        durability_mode: 0,
        commit_tier: 1,
        reader_drain_timeout_ms: 1,
        max_resident_bytes: u64::MAX,
        max_temp_bytes: u64::MAX,
    };
    let mut handle = 0;
    let code = match cell {
        Cell::NullPointer => ze_open(std::ptr::null(), &mut handle),
        Cell::InvalidUtf8Path => {
            let bytes = [0xff_u8];
            request.path = bytes.as_ptr();
            request.path_len = bytes.len();
            ze_open(&request, &mut handle)
        }
        Cell::InteriorNulPath => {
            let bytes = b"bad\0path";
            request.path = bytes.as_ptr();
            request.path_len = bytes.len();
            ze_open(&request, &mut handle)
        }
        Cell::BadEnumDiscriminant => {
            request.access_mode = i32::MAX;
            ze_open(&request, &mut handle)
        }
        Cell::UndersizedAbi => {
            request.abi_size -= 1;
            ze_open(&request, &mut handle)
        }
        Cell::OversizedAbi => {
            request.abi_size = ZE_ABI_MAX_STRUCT_SIZE + 1;
            ze_open(&request, &mut handle)
        }
        Cell::WrongDimensions
        | Cell::ZeroK
        | Cell::HugeK
        | Cell::MisalignedBuffer
        | Cell::CountLengthOverflow => {
            return not_applicable("ZeOpenRequest has no vector, k, or multi-byte count buffer");
        }
    };
    if code == ZeErrorCode::ZeOk {
        let _ = ze_close(handle);
    }
    expected_error(code)
}

fn open_with_epoch_cell(context: &MatrixContext, cell: Cell) -> CellResult {
    let valid_path = context.path_bytes();
    let mut open = ZeOpenRequest {
        abi_size: size_of::<ZeOpenRequest>() as u32,
        abi_reserved: 0,
        path: valid_path.as_ptr(),
        path_len: valid_path.len(),
        access_mode: 1,
        durability_mode: 0,
        commit_tier: 1,
        reader_drain_timeout_ms: 1,
        max_resident_bytes: u64::MAX,
        max_temp_bytes: u64::MAX,
    };
    let fixture = common::EpochFixture::new(1);
    let mut epoch = fixture.request();
    let code = match cell {
        Cell::NullPointer => call_open_with_epoch(std::ptr::null(), &epoch),
        Cell::WrongDimensions => {
            return not_applicable("document and query epoch dimensions may differ");
        }
        Cell::MisalignedBuffer => {
            let bytes = vec![0_u8; size_of::<ZeEpochRequest>() + 8];
            let misaligned = unsafe { bytes.as_ptr().add(1).cast::<ZeEpochRequest>() };
            call_open_with_epoch(&open, misaligned)
        }
        Cell::InvalidUtf8Path => {
            let bytes = [0xff_u8];
            open.path = bytes.as_ptr();
            open.path_len = bytes.len();
            call_open_with_epoch(&open, &epoch)
        }
        Cell::InteriorNulPath => {
            let bytes = b"bad\0path";
            open.path = bytes.as_ptr();
            open.path_len = bytes.len();
            call_open_with_epoch(&open, &epoch)
        }
        Cell::BadEnumDiscriminant => {
            epoch.embedding.document.runtime = i32::MAX;
            call_open_with_epoch(&open, &epoch)
        }
        Cell::UndersizedAbi => {
            open.abi_size -= 1;
            call_open_with_epoch(&open, &epoch)
        }
        Cell::OversizedAbi => {
            open.abi_size = ZE_ABI_MAX_STRUCT_SIZE + 1;
            call_open_with_epoch(&open, &epoch)
        }
        Cell::CountLengthOverflow => {
            epoch.embedding.alignment_digest = std::ptr::NonNull::<u8>::dangling().as_ptr();
            epoch.embedding.alignment_digest_len = usize::MAX;
            call_open_with_epoch(&open, &epoch)
        }
        Cell::ZeroK | Cell::HugeK => {
            return not_applicable("ZeOpenRequest and ZeEpochRequest have no k field");
        }
    };
    expected_error(code)
}

fn call_open_with_epoch(open: *const ZeOpenRequest, epoch: *const ZeEpochRequest) -> ZeErrorCode {
    let mut handle = 0;
    let code = ze_open_with_epoch(open, epoch, &mut handle);
    if code == ZeErrorCode::ZeOk {
        assert_eq!(ze_close(handle), ZeErrorCode::ZeOk);
    }
    code
}

type EpochMatrixCall = fn(&MatrixContext, *const ZeEpochRequest) -> ZeErrorCode;

fn epoch_request_cell(context: &MatrixContext, cell: Cell, call: EpochMatrixCall) -> CellResult {
    let fixture = common::EpochFixture::new(1);
    let mut request = fixture.request();
    let code = match cell {
        Cell::NullPointer => call(context, std::ptr::null()),
        Cell::WrongDimensions => {
            return not_applicable("document and query epoch dimensions may differ");
        }
        Cell::MisalignedBuffer => {
            let bytes = vec![0_u8; size_of::<ZeEpochRequest>() + 8];
            let misaligned = unsafe { bytes.as_ptr().add(1).cast::<ZeEpochRequest>() };
            call(context, misaligned)
        }
        Cell::InvalidUtf8Path => {
            let bytes = [0xff_u8];
            request.embedding.query.model_id = bytes.as_ptr();
            request.embedding.query.model_id_len = bytes.len();
            call(context, &request)
        }
        Cell::BadEnumDiscriminant => {
            request.embedding.document.runtime = i32::MAX;
            call(context, &request)
        }
        Cell::UndersizedAbi => {
            request.abi_size -= 1;
            call(context, &request)
        }
        Cell::OversizedAbi => {
            request.abi_size = ZE_ABI_MAX_STRUCT_SIZE + 1;
            call(context, &request)
        }
        Cell::CountLengthOverflow => {
            request.embedding.alignment_digest = std::ptr::NonNull::<u8>::dangling().as_ptr();
            request.embedding.alignment_digest_len = usize::MAX;
            call(context, &request)
        }
        Cell::ZeroK | Cell::HugeK | Cell::InteriorNulPath => {
            return not_applicable("ZeEpochRequest has no k or path field");
        }
    };
    expected_error(code)
}

fn epoch_identity_cell(context: &MatrixContext, cell: Cell) -> CellResult {
    epoch_request_cell(context, cell, |_, request| {
        let mut identity: ZeEpochIdentity = common::sized_zeroed();
        ze_epoch_identity(request, &mut identity)
    })
}

fn epoch_drop_cell(context: &MatrixContext, cell: Cell) -> CellResult {
    epoch_request_cell(context, cell, |context, request| {
        let mut report: ZeEpochDropReport = common::sized_zeroed();
        ze_epoch_drop(context.store.handle, request, &mut report)
    })
}

fn ingest_cell(context: &MatrixContext, cell: Cell) -> CellResult {
    let mut document = ZeIngestDocument {
        abi_size: size_of::<ZeIngestDocument>() as u32,
        abi_reserved: 0,
        doc_id: context.ids[0],
        revision: 1,
        timestamp: 0,
        vector: context.vector.as_ptr(),
        vector_len: context.vector.len(),
        metadata: std::ptr::null(),
        metadata_len: 0,
        text: std::ptr::null(),
        text_len: 0,
    };
    let mut request = ZeIngestRequest {
        abi_size: size_of::<ZeIngestRequest>() as u32,
        abi_reserved: 0,
        documents: &document,
        document_count: 1,
        dimension: context.vector.len(),
    };
    let mut report: ZeMutationReport = common::sized_zeroed();
    let code = match cell {
        Cell::NullPointer => ze_ingest(context.store.handle, std::ptr::null(), &mut report),
        Cell::WrongDimensions => {
            request.dimension += 1;
            ze_ingest(context.store.handle, &request, &mut report)
        }
        Cell::MisalignedBuffer => {
            let aligned = [0_u32; 2];
            document.vector = unsafe { aligned.as_ptr().cast::<u8>().add(1).cast::<f32>() };
            request.documents = &document;
            ze_ingest(context.store.handle, &request, &mut report)
        }
        Cell::UndersizedAbi => {
            request.abi_size -= 1;
            ze_ingest(context.store.handle, &request, &mut report)
        }
        Cell::OversizedAbi => {
            request.abi_size = ZE_ABI_MAX_STRUCT_SIZE + 1;
            ze_ingest(context.store.handle, &request, &mut report)
        }
        Cell::CountLengthOverflow => {
            request.documents = std::ptr::NonNull::<ZeIngestDocument>::dangling().as_ptr();
            request.document_count = usize::MAX;
            ze_ingest(context.store.handle, &request, &mut report)
        }
        Cell::ZeroK
        | Cell::HugeK
        | Cell::InvalidUtf8Path
        | Cell::InteriorNulPath
        | Cell::BadEnumDiscriminant => {
            return not_applicable("ZeIngestRequest has no k, path, or enum field");
        }
    };
    expected_error(code)
}

fn delete_cell(context: &MatrixContext, cell: Cell) -> CellResult {
    let mut request = ZeDeleteRequest {
        abi_size: size_of::<ZeDeleteRequest>() as u32,
        abi_reserved: 0,
        doc_ids: context.ids.as_ptr(),
        doc_id_count: context.ids.len(),
    };
    let mut report: ZeMutationReport = common::sized_zeroed();
    let code = match cell {
        Cell::NullPointer => ze_delete(context.store.handle, std::ptr::null(), &mut report),
        Cell::MisalignedBuffer => {
            let aligned = [0_u64; 3];
            request.doc_ids = unsafe { aligned.as_ptr().cast::<u8>().add(1).cast::<ZeDocId>() };
            ze_delete(context.store.handle, &request, &mut report)
        }
        Cell::UndersizedAbi => {
            request.abi_size -= 1;
            ze_delete(context.store.handle, &request, &mut report)
        }
        Cell::OversizedAbi => {
            request.abi_size = ZE_ABI_MAX_STRUCT_SIZE + 1;
            ze_delete(context.store.handle, &request, &mut report)
        }
        Cell::CountLengthOverflow => {
            request.doc_ids = std::ptr::NonNull::<ZeDocId>::dangling().as_ptr();
            request.doc_id_count = usize::MAX;
            ze_delete(context.store.handle, &request, &mut report)
        }
        Cell::WrongDimensions
        | Cell::ZeroK
        | Cell::HugeK
        | Cell::InvalidUtf8Path
        | Cell::InteriorNulPath
        | Cell::BadEnumDiscriminant => {
            return not_applicable("ZeDeleteRequest has no dimension, k, path, or enum field");
        }
    };
    expected_error(code)
}

fn search_cell(context: &MatrixContext, cell: Cell) -> CellResult {
    let mut request = common::valid_search_request(&context.vector);
    let mut result: ZeSearchResult = common::sized_zeroed();
    let code = match cell {
        Cell::NullPointer => ze_search(context.store.handle, std::ptr::null(), &mut result),
        Cell::WrongDimensions => {
            request.dimension += 1;
            ze_search(context.store.handle, &request, &mut result)
        }
        Cell::ZeroK => {
            request.k = 0;
            ze_search(context.store.handle, &request, &mut result)
        }
        Cell::HugeK => {
            request.k = ZE_MAX_K + 1;
            ze_search(context.store.handle, &request, &mut result)
        }
        Cell::MisalignedBuffer => {
            let aligned = [0_u32; 2];
            request.vector = unsafe { aligned.as_ptr().cast::<u8>().add(1).cast::<f32>() };
            ze_search(context.store.handle, &request, &mut result)
        }
        Cell::BadEnumDiscriminant => {
            request.has_tier = 1;
            request.tier = i32::MAX;
            ze_search(context.store.handle, &request, &mut result)
        }
        Cell::UndersizedAbi => {
            request.abi_size -= 1;
            ze_search(context.store.handle, &request, &mut result)
        }
        Cell::OversizedAbi => {
            request.abi_size = ZE_ABI_MAX_STRUCT_SIZE + 1;
            ze_search(context.store.handle, &request, &mut result)
        }
        Cell::CountLengthOverflow => {
            request.vector = std::ptr::NonNull::<f32>::dangling().as_ptr();
            request.vector_len = usize::MAX;
            request.dimension = usize::MAX;
            ze_search(context.store.handle, &request, &mut result)
        }
        Cell::InvalidUtf8Path | Cell::InteriorNulPath => {
            return not_applicable("ZeSearchRequest has no path field");
        }
    };
    expected_error(code)
}

fn query_cell(context: &MatrixContext, cell: Cell) -> CellResult {
    let mut request = common::valid_query_request(&context.vector);
    let mut result: ZeQueryResult = common::sized_zeroed();
    let code = match cell {
        Cell::NullPointer => ze_query(context.store.handle, std::ptr::null(), &mut result),
        Cell::WrongDimensions => {
            request.dimension += 1;
            ze_query(context.store.handle, &request, &mut result)
        }
        Cell::ZeroK => {
            request.k = 0;
            ze_query(context.store.handle, &request, &mut result)
        }
        Cell::HugeK => {
            request.k = ZE_MAX_K + 1;
            ze_query(context.store.handle, &request, &mut result)
        }
        Cell::MisalignedBuffer => {
            let aligned = [0_u32; 2];
            request.vector = unsafe { aligned.as_ptr().cast::<u8>().add(1).cast::<f32>() };
            ze_query(context.store.handle, &request, &mut result)
        }
        Cell::BadEnumDiscriminant => {
            request.has_tier = 1;
            request.tier = i32::MAX;
            ze_query(context.store.handle, &request, &mut result)
        }
        Cell::UndersizedAbi => {
            request.abi_size -= 1;
            ze_query(context.store.handle, &request, &mut result)
        }
        Cell::OversizedAbi => {
            request.abi_size = ZE_ABI_MAX_STRUCT_SIZE + 1;
            ze_query(context.store.handle, &request, &mut result)
        }
        Cell::CountLengthOverflow => {
            request.vector = std::ptr::NonNull::<f32>::dangling().as_ptr();
            request.vector_len = usize::MAX;
            request.dimension = usize::MAX;
            ze_query(context.store.handle, &request, &mut result)
        }
        Cell::InvalidUtf8Path => {
            // The lexical leg is the query surface's text buffer.
            let bytes = [0xff_u8];
            request.text = bytes.as_ptr();
            request.text_len = bytes.len();
            ze_query(context.store.handle, &request, &mut result)
        }
        Cell::InteriorNulPath => {
            return not_applicable("query text is opaque UTF-8, not a path");
        }
    };
    expected_error(code)
}

fn epoch_alias_cell(context: &MatrixContext, cell: Cell) -> CellResult {
    let fixture = common::EpochFixture::new(1);
    let mut request = fixture.request();
    let mut report: ZeEpochAliasReport = common::sized_zeroed();
    let code = match cell {
        Cell::NullPointer => {
            ze_epoch_switch_alias(context.store.handle, std::ptr::null(), &mut report)
        }
        Cell::MisalignedBuffer => {
            // u8 buffers are always aligned; a misaligned request struct is the
            // only alignment fault this surface can observe.
            let bytes = vec![0_u8; std::mem::size_of::<ZeEpochRequest>() + 8];
            let misaligned = unsafe { bytes.as_ptr().add(1).cast::<ZeEpochRequest>() };
            ze_epoch_switch_alias(context.store.handle, misaligned, &mut report)
        }
        Cell::InvalidUtf8Path => {
            let bytes = [0xff_u8];
            request.embedding.query.model_id = bytes.as_ptr();
            request.embedding.query.model_id_len = bytes.len();
            ze_epoch_switch_alias(context.store.handle, &request, &mut report)
        }
        Cell::BadEnumDiscriminant => {
            request.embedding.document.runtime = i32::MAX;
            ze_epoch_switch_alias(context.store.handle, &request, &mut report)
        }
        Cell::UndersizedAbi => {
            request.abi_size -= 1;
            ze_epoch_switch_alias(context.store.handle, &request, &mut report)
        }
        Cell::OversizedAbi => {
            request.abi_size = ZE_ABI_MAX_STRUCT_SIZE + 1;
            ze_epoch_switch_alias(context.store.handle, &request, &mut report)
        }
        Cell::CountLengthOverflow => {
            request.embedding.alignment_digest = std::ptr::NonNull::<u8>::dangling().as_ptr();
            request.embedding.alignment_digest_len = usize::MAX;
            ze_epoch_switch_alias(context.store.handle, &request, &mut report)
        }
        Cell::WrongDimensions | Cell::ZeroK | Cell::HugeK | Cell::InteriorNulPath => {
            return not_applicable("ZeEpochRequest has no vector, k, or path field");
        }
    };
    expected_error(code)
}

macro_rules! header_only_cell {
    ($context:expr, $cell:expr, $request:expr, $report:expr, $call:expr, $reason:literal) => {{
        let mut request = $request;
        let mut report = $report;
        let code = match $cell {
            Cell::NullPointer => $call($context.store.handle, std::ptr::null(), &mut report),
            Cell::UndersizedAbi => {
                request.abi_size -= 1;
                $call($context.store.handle, &request, &mut report)
            }
            Cell::OversizedAbi => {
                request.abi_size = ZE_ABI_MAX_STRUCT_SIZE + 1;
                $call($context.store.handle, &request, &mut report)
            }
            _ => return not_applicable($reason),
        };
        expected_error(code)
    }};
}

fn seal_cell(context: &MatrixContext, cell: Cell) -> CellResult {
    header_only_cell!(
        context,
        cell,
        ZeSealRequest {
            abi_size: size_of::<ZeSealRequest>() as u32,
            abi_reserved: 0,
            cancel_token: 0,
        },
        common::sized_zeroed::<ZeGenerationReport>(),
        ze_seal,
        "ZeSealRequest has no dimension, k, path, enum, or count buffer"
    )
}

fn drop_cell(context: &MatrixContext, cell: Cell) -> CellResult {
    header_only_cell!(
        context,
        cell,
        ZeDropPartitionRequest {
            abi_size: size_of::<ZeDropPartitionRequest>() as u32,
            abi_reserved: 0,
            start_ts: 0,
            end_ts: 1,
        },
        common::sized_zeroed::<ZePartitionReport>(),
        ze_drop_partition,
        "ZeDropPartitionRequest has no dimension, k, path, enum, or count buffer"
    )
}

fn retention_cell(context: &MatrixContext, cell: Cell) -> CellResult {
    header_only_cell!(
        context,
        cell,
        ZeRetentionRequest {
            abi_size: size_of::<ZeRetentionRequest>() as u32,
            abi_reserved: 0,
            window: 1,
            now_ts: 1,
        },
        common::sized_zeroed::<ZePartitionReport>(),
        ze_apply_retention,
        "ZeRetentionRequest has no dimension, k, path, enum, or count buffer"
    )
}

fn purge_cell(context: &MatrixContext, cell: Cell) -> CellResult {
    let mut request = ZePurgeRequest {
        abi_size: size_of::<ZePurgeRequest>() as u32,
        abi_reserved: 0,
        doc_ids: context.ids.as_ptr(),
        doc_id_count: context.ids.len(),
    };
    let mut report: ZePurgeTokenReport = common::sized_zeroed();
    let code = match cell {
        Cell::NullPointer => ze_purge(context.store.handle, std::ptr::null(), &mut report),
        Cell::MisalignedBuffer => {
            let aligned = [0_u64; 3];
            request.doc_ids = unsafe { aligned.as_ptr().cast::<u8>().add(1).cast::<ZeDocId>() };
            ze_purge(context.store.handle, &request, &mut report)
        }
        Cell::UndersizedAbi => {
            request.abi_size -= 1;
            ze_purge(context.store.handle, &request, &mut report)
        }
        Cell::OversizedAbi => {
            request.abi_size = ZE_ABI_MAX_STRUCT_SIZE + 1;
            ze_purge(context.store.handle, &request, &mut report)
        }
        Cell::CountLengthOverflow => {
            request.doc_ids = std::ptr::NonNull::<ZeDocId>::dangling().as_ptr();
            request.doc_id_count = usize::MAX;
            ze_purge(context.store.handle, &request, &mut report)
        }
        Cell::WrongDimensions
        | Cell::ZeroK
        | Cell::HugeK
        | Cell::InvalidUtf8Path
        | Cell::InteriorNulPath
        | Cell::BadEnumDiscriminant => {
            return not_applicable("ZePurgeRequest has no dimension, k, path, or enum field");
        }
    };
    expected_error(code)
}

fn await_cell(context: &MatrixContext, cell: Cell) -> CellResult {
    header_only_cell!(
        context,
        cell,
        ZeAwaitPurgeRequest {
            abi_size: size_of::<ZeAwaitPurgeRequest>() as u32,
            abi_reserved: 0,
            token_id: 1,
        },
        common::sized_zeroed::<ZePurgeReport>(),
        ze_await_physical_purge,
        "ZeAwaitPurgeRequest has no dimension, k, path, enum, or count buffer"
    )
}

fn maintain_cell(context: &MatrixContext, cell: Cell) -> CellResult {
    header_only_cell!(
        context,
        cell,
        ZeMaintainRequest {
            abi_size: size_of::<ZeMaintainRequest>() as u32,
            abi_reserved: 0,
            wall_time_ns: 1,
            bytes: 1,
        },
        common::sized_zeroed::<ZeMaintainReport>(),
        ze_maintain,
        "ZeMaintainRequest has no dimension, k, path, enum, or count buffer"
    )
}

impl MatrixContext {
    fn path_bytes(&self) -> Vec<u8> {
        self.store.path.to_string_lossy().into_owned().into_bytes()
    }
}

fn probe_abi_version() -> bool {
    ze_abi_version() > 0
}

fn probe_error_code_name(_: &MatrixContext) -> ProbeResult {
    ProbeResult::Pointer(ze_error_code_name(i32::MAX))
}

fn probe_epoch_current(context: &MatrixContext) -> ProbeResult {
    ProbeResult::Status(ze_epoch_current(context.store.handle, std::ptr::null_mut()))
}

fn probe_close(_: &MatrixContext) -> ProbeResult {
    ProbeResult::Status(ze_close(u64::MAX))
}

fn probe_state(context: &MatrixContext) -> ProbeResult {
    ProbeResult::Status(ze_state(context.store.handle, std::ptr::null_mut()))
}

fn probe_stats(context: &MatrixContext) -> ProbeResult {
    ProbeResult::Status(ze_stats(context.store.handle, std::ptr::null_mut()))
}

fn probe_upsert(context: &MatrixContext) -> ProbeResult {
    ProbeResult::Status(ze_upsert(
        context.store.handle,
        std::ptr::null(),
        std::ptr::null_mut(),
    ))
}

fn probe_query_result_free(_: &MatrixContext) -> ProbeResult {
    ProbeResult::Status(ze_query_result_free(std::ptr::null_mut()))
}

fn probe_namespace_open(_: &MatrixContext) -> ProbeResult {
    ProbeResult::Status(ze_namespace_open(std::ptr::null(), std::ptr::null_mut()))
}

fn probe_namespace_list(_: &MatrixContext) -> ProbeResult {
    ProbeResult::Status(ze_namespace_list(std::ptr::null(), std::ptr::null_mut()))
}

fn probe_namespace_list_result_free(_: &MatrixContext) -> ProbeResult {
    ProbeResult::Status(ze_namespace_list_result_free(std::ptr::null_mut()))
}

fn probe_last_error_message(context: &MatrixContext) -> ProbeResult {
    ProbeResult::Status(ze_last_error_message(
        context.store.handle,
        std::ptr::null_mut(),
        0,
        std::ptr::null_mut(),
    ))
}

fn probe_cancel_create(_: &MatrixContext) -> ProbeResult {
    ProbeResult::Status(ze_cancel_token_create(std::ptr::null_mut()))
}

fn probe_cancel_cancel(_: &MatrixContext) -> ProbeResult {
    ProbeResult::Status(ze_cancel_token_cancel(0))
}

fn probe_cancel_free(_: &MatrixContext) -> ProbeResult {
    ProbeResult::Status(ze_cancel_token_free(0))
}

fn probe_search_result_free(_: &MatrixContext) -> ProbeResult {
    ProbeResult::Status(ze_search_result_free(std::ptr::null_mut()))
}

#[cfg(feature = "text")]
fn probe_text_open(_: &MatrixContext) -> ProbeResult {
    ProbeResult::Status(ze_text_open(std::ptr::null(), std::ptr::null_mut()))
}

#[cfg(feature = "text")]
fn probe_text_ingest(_: &MatrixContext) -> ProbeResult {
    ProbeResult::Status(ze_text_ingest(
        u64::MAX,
        std::ptr::null(),
        std::ptr::null_mut(),
    ))
}

#[cfg(feature = "text")]
fn probe_text_maintain(_: &MatrixContext) -> ProbeResult {
    ProbeResult::Status(ze_text_maintain(
        u64::MAX,
        std::ptr::null(),
        std::ptr::null_mut(),
    ))
}

#[cfg(feature = "text")]
fn probe_text_query(_: &MatrixContext) -> ProbeResult {
    ProbeResult::Status(ze_text_query(
        u64::MAX,
        std::ptr::null(),
        std::ptr::null_mut(),
    ))
}

#[cfg(feature = "text")]
fn probe_text_query_result_free(_: &MatrixContext) -> ProbeResult {
    ProbeResult::Status(ze_text_query_result_free(std::ptr::null_mut()))
}

#[test]
fn every_exported_symbol_has_executable_adversarial_registry_coverage() {
    let exported = include_str!("../src/lib.rs")
        .lines()
        .filter_map(|line| {
            line.trim_start()
                .strip_prefix("pub extern \"C\" fn ")
                .and_then(|tail| tail.split('(').next())
                .filter(|name| cfg!(feature = "text") || !name.starts_with("ze_text_"))
                .map(str::to_owned)
        })
        .collect::<std::collections::BTreeSet<_>>();
    let registered = ABI_REGISTRY
        .iter()
        .map(|entry| entry.name.to_owned())
        .collect::<std::collections::BTreeSet<_>>();
    assert_eq!(
        registered.len(),
        ABI_REGISTRY.len(),
        "duplicate ABI registry entry"
    );
    assert_eq!(
        registered, exported,
        "ABI exports and adversarial registry drifted"
    );

    let detailed = DETAILED_MATRIX
        .iter()
        .map(|(name, _)| *name)
        .collect::<std::collections::BTreeSet<_>>();
    let context = MatrixContext {
        store: common::TestStore::new(),
        vector: vec![1.0_f32],
        ids: vec![ZeDocId { high: 0, low: 1 }],
    };
    for entry in ABI_REGISTRY {
        match entry.coverage {
            AbiCoverage::DetailedMatrix => assert!(
                detailed.contains(entry.name),
                "{} claims a detailed matrix but has no matrix call",
                entry.name
            ),
            AbiCoverage::InvalidProbe(probe) => match probe(&context) {
                ProbeResult::Status(code) => {
                    assert_ne!(code, ZeErrorCode::ZeOk, "{} invalid probe", entry.name);
                    assert_ne!(code, ZeErrorCode::ZeErrPanic, "{} panicked", entry.name);
                    assert!(
                        (1..=34).contains(&(code as i32)),
                        "{} typed code",
                        entry.name
                    );
                }
                ProbeResult::Pointer(pointer) => {
                    assert!(
                        !pointer.is_null(),
                        "{} returned a null static value",
                        entry.name
                    );
                }
            },
            AbiCoverage::ValueProbe(probe) => {
                assert!(probe(), "{} value probe failed", entry.name);
            }
        }
    }
}

#[test]
fn the_adversarial_input_matrix_returns_typed_errors_for_every_cell() {
    const EXPECTED_EXECUTED: usize = 85;
    const EXPECTED_SKIPPED: usize = 82;
    let context = MatrixContext {
        store: common::TestStore::new(),
        vector: vec![1.0_f32],
        ids: vec![ZeDocId { high: 0, low: 1 }],
    };
    let mut executed = 0;
    let mut skipped = Vec::new();
    for (operation, call) in DETAILED_MATRIX {
        for cell in CELLS {
            match call(&context, *cell) {
                CellResult::Executed(code) => {
                    assert_ne!(code, ZeErrorCode::ZeOk, "{operation} {cell:?}");
                    assert!((1..=28).contains(&(code as i32)), "typed error code");
                    executed += 1;
                }
                CellResult::Skipped(reason) => skipped.push(format!(
                    "{operation} {cell:?}: not applicable because {reason}"
                )),
            }
        }
    }
    skipped.push(
        "sleep/wake simulation: skipped because the C ABI exposes no clock-injection seam"
            .to_owned(),
    );
    skipped.push(
        "permission failure: skipped because the C ABI exposes no VFS-injection seam".to_owned(),
    );
    for reason in &skipped {
        eprintln!("COUNTED_SKIP {reason}");
    }
    assert_eq!(executed, EXPECTED_EXECUTED);
    assert_eq!(skipped.len(), EXPECTED_SKIPPED);
}

#[test]
fn concurrent_close_and_search_and_concurrent_close_and_ingest_are_typed_never_ub() {
    let search_store = common::TestStore::new();
    assert_eq!(
        common::ingest_rows(search_store.handle, 20, 128),
        ZeErrorCode::ZeOk
    );
    let search_handle = search_store.handle;
    let (ready_tx, ready_rx) = std::sync::mpsc::channel();
    let search = std::thread::spawn(move || {
        let vector = vec![0.5_f32; 128];
        let request = common::valid_search_request(&vector);
        let mut result: ZeSearchResult = common::sized_zeroed();
        ready_tx.send(()).expect("search ready");
        let code = ze_search(search_handle, &request, &mut result);
        if code == ZeErrorCode::ZeOk {
            assert_eq!(ze_search_result_free(&mut result), ZeErrorCode::ZeOk);
        }
        code
    });
    ready_rx.recv().expect("search ready");
    assert_eq!(ze_close(search_handle), ZeErrorCode::ZeOk);
    assert!(matches!(
        search.join().expect("search thread"),
        ZeErrorCode::ZeOk
            | ZeErrorCode::ZeErrClosed
            | ZeErrorCode::ZeErrClosing
            | ZeErrorCode::ZeErrCancelled
    ));

    let ingest_store = common::TestStore::new();
    let ingest_handle = ingest_store.handle;
    let (ready_tx, ready_rx) = std::sync::mpsc::channel();
    let ingest = std::thread::spawn(move || {
        ready_tx.send(()).expect("ingest ready");
        common::ingest_rows(ingest_handle, 20, 64)
    });
    ready_rx.recv().expect("ingest ready");
    assert_eq!(ze_close(ingest_handle), ZeErrorCode::ZeOk);
    assert!(matches!(
        ingest.join().expect("ingest thread"),
        ZeErrorCode::ZeOk | ZeErrorCode::ZeErrClosed | ZeErrorCode::ZeErrClosing
    ));
}

#[test]
fn two_handles_on_the_same_path_in_one_process_are_typed() {
    let store = common::TestStore::new();
    let (code, second) = common::open_path(&store.path);
    assert_eq!(code, ZeErrorCode::ZeErrStoreBusy);
    assert_eq!(second, 0);
    let mut state: ZeStateReport = common::sized_zeroed();
    assert_eq!(ze_state(store.handle, &mut state), ZeErrorCode::ZeOk);
}
