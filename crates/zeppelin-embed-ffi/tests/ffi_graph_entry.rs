mod common;
use common::graph::*;
use zeppelin_embed_ffi::*;

#[test]
fn graph_open_creates_reopens_and_closes_a_store() {
    let mut store = GraphTestStore::create();
    assert_eq!(store.close(), ZeErrorCode::ZeOk);
    for mode in [MODE_READ_WRITE, MODE_READ_ONLY] {
        let (code, handle) = graph_open(&store.path, mode);
        assert_eq!(code, ZeErrorCode::ZeOk);
        assert_eq!(ze_graph_close(handle), ZeErrorCode::ZeOk);
    }
}
#[test]
fn graph_open_refuses_a_legacy_store_directory_with_store_kind() {
    let mut store = common::TestStore::new();
    assert_eq!(store.close(), ZeErrorCode::ZeOk);
    assert_eq!(
        graph_open(&store.path, MODE_READ_WRITE).0,
        ZeErrorCode::ZeErrStoreKind
    );
    assert!(last_error(0).contains(store.path.to_str().unwrap()));
}
#[test]
fn graph_open_rejects_each_malformed_request_before_touching_the_disk() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("untouched");
    let bytes = path.to_str().unwrap().as_bytes();
    for case in 0..11 {
        let mut request = open_request(bytes, MODE_CREATE);
        let mut handle = ZeGraphHandle { token: 0 };
        let control: ZeGraphControl = common::sized_zeroed();
        match case {
            0 => {
                request.path = ZeGraphBytes {
                    data: std::ptr::null(),
                    count: 0,
                }
            }
            1 => {
                request.path = ZeGraphBytes {
                    data: b"a\0b".as_ptr(),
                    count: 3,
                }
            }
            2 => {
                request.path = ZeGraphBytes {
                    data: b"\xff".as_ptr(),
                    count: 1,
                }
            }
            3 => request.tokenizer_profile = 1,
            4 => request.max_resident_bytes = 0,
            5 => request.max_resident_bytes += 1,
            6 => request.mode = 3,
            7 => request.abi_size += 8,
            8 => request.abi_reserved = 1,
            9 => {}
            10 => request.control = &control,
            _ => unreachable!(),
        }
        let out = if case == 9 {
            std::ptr::null_mut()
        } else {
            &mut handle
        };
        let expected = if case == 10 {
            ZeErrorCode::ZeErrUnsupported
        } else {
            ZeErrorCode::ZeErrInvalidArgument
        };
        assert_eq!(ze_graph_open(&request, out), expected, "case {case}");
        assert!(!path.exists(), "case {case} touched disk");
    }
}
#[test]
fn graph_handles_are_distinct_from_legacy_and_text_handles() {
    let graph = GraphTestStore::create();
    let legacy = common::TestStore::new();
    assert_eq!(
        ze_close(graph.handle.token),
        ZeErrorCode::ZeErrInvalidHandle
    );
    for token in [legacy.handle, 0, graph.handle.token | (1 << 31)] {
        assert_eq!(
            ze_graph_close(ZeGraphHandle { token }),
            ZeErrorCode::ZeErrInvalidHandle
        );
    }
}
#[test]
fn graph_close_twice_and_a_stale_generation_return_typed_errors() {
    let mut store = GraphTestStore::create();
    let old = store.handle;
    assert_eq!(store.close(), ZeErrorCode::ZeOk);
    assert_eq!(ze_graph_close(old), ZeErrorCode::ZeErrClosed);
    let (code, new) = graph_open(&store.path, MODE_READ_WRITE);
    assert_eq!(code, ZeErrorCode::ZeOk);
    assert_ne!(old.token, new.token);
    assert_eq!(ze_graph_close(old), ZeErrorCode::ZeErrClosed);
    assert_eq!(ze_graph_close(new), ZeErrorCode::ZeOk);
}
#[test]
fn graph_last_error_message_routes_by_graph_token() {
    let graph = GraphTestStore::create();
    let mut legacy = common::TestStore::new();
    assert_eq!(legacy.close(), ZeErrorCode::ZeOk);
    assert_eq!(
        graph_open(&legacy.path, MODE_READ_WRITE).0,
        ZeErrorCode::ZeErrStoreKind
    );
    let global = last_error(0);
    // A wrong-kind close fails while the graph token still identifies a live slot.
    assert_eq!(
        ze_close(graph.handle.token),
        ZeErrorCode::ZeErrInvalidHandle
    );
    assert!(!last_error(graph.handle.token).is_empty());
    assert_eq!(last_error(0), global);
}
