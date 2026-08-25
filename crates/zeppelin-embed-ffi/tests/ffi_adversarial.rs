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
    if code == ZeErrorCode::Ok {
        let _ = ze_close(handle);
    }
    expected_error(code)
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
            request.search_tier = i32::MAX;
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

#[test]
fn the_adversarial_input_matrix_returns_typed_errors_for_every_cell() {
    const EXPECTED_EXECUTED: usize = 46;
    const EXPECTED_SKIPPED: usize = 66;
    let context = MatrixContext {
        store: common::TestStore::new(),
        vector: vec![1.0_f32],
        ids: vec![ZeDocId { high: 0, low: 1 }],
    };
    let operations: &[(&str, MatrixCall)] = &[
        ("ze_open", open_cell),
        ("ze_ingest", ingest_cell),
        ("ze_delete", delete_cell),
        ("ze_search", search_cell),
        ("ze_seal", seal_cell),
        ("ze_drop_partition", drop_cell),
        ("ze_apply_retention", retention_cell),
        ("ze_purge", purge_cell),
        ("ze_await_physical_purge", await_cell),
        ("ze_maintain", maintain_cell),
    ];
    let mut executed = 0;
    let mut skipped = Vec::new();
    for (operation, call) in operations {
        for cell in CELLS {
            match call(&context, *cell) {
                CellResult::Executed(code) => {
                    assert_ne!(code, ZeErrorCode::Ok, "{operation} {cell:?}");
                    assert!((1..=25).contains(&(code as i32)), "typed error code");
                    executed += 1;
                }
                CellResult::Skipped(reason) => skipped.push(format!(
                    "{operation} {cell:?}: not applicable because {reason}"
                )),
            }
        }
    }
    skipped.push(
        "sleep/wake simulation: skipped because the repository exposes no clock-jump seam"
            .to_owned(),
    );
    skipped.push(
        "permission failure: skipped because a portable permission-denial seam is unavailable"
            .to_owned(),
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
        ZeErrorCode::Ok
    );
    let search_handle = search_store.handle;
    let (ready_tx, ready_rx) = std::sync::mpsc::channel();
    let search = std::thread::spawn(move || {
        let vector = vec![0.5_f32; 128];
        let request = common::valid_search_request(&vector);
        let mut result: ZeSearchResult = common::sized_zeroed();
        ready_tx.send(()).expect("search ready");
        let code = ze_search(search_handle, &request, &mut result);
        if code == ZeErrorCode::Ok {
            assert_eq!(ze_search_result_free(&mut result), ZeErrorCode::Ok);
        }
        code
    });
    ready_rx.recv().expect("search ready");
    assert_eq!(ze_close(search_handle), ZeErrorCode::Ok);
    assert!(matches!(
        search.join().expect("search thread"),
        ZeErrorCode::Ok | ZeErrorCode::Closed | ZeErrorCode::Closing | ZeErrorCode::Cancelled
    ));

    let ingest_store = common::TestStore::new();
    let ingest_handle = ingest_store.handle;
    let (ready_tx, ready_rx) = std::sync::mpsc::channel();
    let ingest = std::thread::spawn(move || {
        ready_tx.send(()).expect("ingest ready");
        common::ingest_rows(ingest_handle, 20, 64)
    });
    ready_rx.recv().expect("ingest ready");
    assert_eq!(ze_close(ingest_handle), ZeErrorCode::Ok);
    assert!(matches!(
        ingest.join().expect("ingest thread"),
        ZeErrorCode::Ok | ZeErrorCode::Closed | ZeErrorCode::Closing
    ));
}

#[test]
fn two_handles_on_the_same_path_in_one_process_are_typed() {
    let store = common::TestStore::new();
    let (code, second) = common::open_path(&store.path);
    assert_eq!(code, ZeErrorCode::StoreBusy);
    assert_eq!(second, 0);
    let mut state: ZeStateReport = common::sized_zeroed();
    assert_eq!(ze_state(store.handle, &mut state), ZeErrorCode::Ok);
}
