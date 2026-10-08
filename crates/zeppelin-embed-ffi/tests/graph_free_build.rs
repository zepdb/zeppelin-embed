#![cfg(not(feature = "graph-cypher"))]
#![allow(clippy::expect_used)]

use zeppelin_embed::format::golden::decode_hex;
use zeppelin_embed_ffi::{ZeErrorCode, ZeOpenRequest, ze_open};

#[test]
fn a_v3_store_is_refused_with_the_stable_c_code_before_any_write() {
    for access_mode in [0, 1] {
        let directory = tempfile::tempdir().expect("directory");
        let bytes = decode_hex(include_str!(
            "../../zeppelin-embed/tests/fixtures/format/manifest_v3.hex"
        ))
        .expect("golden");
        std::fs::write(directory.path().join("manifest.ze"), &bytes).expect("manifest");
        let path = directory.path().to_string_lossy().into_owned();
        let request = ZeOpenRequest {
            abi_size: std::mem::size_of::<ZeOpenRequest>() as u32,
            abi_reserved: 0,
            path: path.as_ptr(),
            path_len: path.len(),
            access_mode,
            durability_mode: 0,
            commit_tier: 1,
            reader_drain_timeout_ms: 250,
            max_resident_bytes: u64::MAX,
            max_temp_bytes: u64::MAX,
        };
        let mut handle = 0;
        assert_eq!(
            ze_open(&request, &mut handle),
            ZeErrorCode::ZeErrGraphUnsupportedBuild
        );
        assert_eq!(handle, 0);
        let files = std::fs::read_dir(directory.path())
            .expect("files")
            .map(|entry| {
                let entry = entry.expect("entry");
                (
                    entry.file_name(),
                    std::fs::read(entry.path()).expect("bytes"),
                )
            })
            .collect::<Vec<_>>();
        assert_eq!(files, vec![("manifest.ze".into(), bytes)]);
    }
}

#[test]
fn graph_calls_refuse_an_unsupported_build_without_touching_pointers() {
    use zeppelin_embed_ffi::*;
    assert_eq!(
        ze_store_enable_graph(0, std::ptr::null_mut()),
        ZeErrorCode::ZeErrGraphUnsupportedBuild
    );
    assert_eq!(
        ze_store_graph_apply(0, std::ptr::null(), std::ptr::null_mut()),
        ZeErrorCode::ZeErrGraphUnsupportedBuild
    );
    assert_eq!(
        ze_store_cypher(0, std::ptr::null(), std::ptr::null_mut()),
        ZeErrorCode::ZeErrGraphUnsupportedBuild
    );
    assert_eq!(
        ze_store_cypher_with_row_limit(0, std::ptr::null(), 1, std::ptr::null_mut()),
        ZeErrorCode::ZeErrGraphUnsupportedBuild
    );
    assert_eq!(
        ze_store_graph_query(0, std::ptr::null(), std::ptr::null_mut()),
        ZeErrorCode::ZeErrGraphUnsupportedBuild
    );
    assert_eq!(
        ze_store_get_nodes(0, std::ptr::null(), std::ptr::null_mut()),
        ZeErrorCode::ZeErrGraphUnsupportedBuild
    );
    assert_eq!(
        ze_store_get_relationships(0, std::ptr::null(), std::ptr::null_mut()),
        ZeErrorCode::ZeErrGraphUnsupportedBuild
    );
    assert_eq!(
        ze_store_graph_resources(0, std::ptr::null_mut()),
        ZeErrorCode::ZeErrGraphUnsupportedBuild
    );
}
