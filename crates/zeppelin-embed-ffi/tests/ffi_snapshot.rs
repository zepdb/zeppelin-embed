//! ZE-220: `ze_snapshot` request validation, target rules and restore.

#![allow(clippy::expect_used, clippy::indexing_slicing, clippy::panic)]

mod common;

use std::mem::size_of;
use std::path::Path;

use zeppelin_embed_ffi::*;

fn request(target: &[u8]) -> ZeSnapshotRequest {
    ZeSnapshotRequest {
        abi_size: size_of::<ZeSnapshotRequest>() as u32,
        abi_reserved: 0,
        target: target.as_ptr(),
        target_len: target.len(),
    }
}

fn path_bytes(path: &Path) -> Vec<u8> {
    path.to_string_lossy().into_owned().into_bytes()
}

fn snapshot(handle: ZeHandle, request: &ZeSnapshotRequest) -> (ZeErrorCode, u64) {
    let mut report: ZeGenerationReport = common::sized_zeroed();
    let code = ze_snapshot(handle, request, &mut report);
    (code, report.generation)
}

fn last_error(handle: ZeHandle) -> String {
    let mut required = 0;
    assert_eq!(
        ze_last_error_message(handle, std::ptr::null_mut(), 0, &mut required),
        ZeErrorCode::ZeOk
    );
    let mut message = vec![0_i8; required + 1];
    assert_eq!(
        ze_last_error_message(handle, message.as_mut_ptr(), message.len(), &mut required),
        ZeErrorCode::ZeOk
    );
    let bytes = message
        .iter()
        .take(required)
        .map(|byte| *byte as u8)
        .collect();
    String::from_utf8(bytes).expect("UTF-8 error")
}

fn count(handle: ZeHandle) -> (u64, u64) {
    let request = ZeCountRequest {
        abi_size: size_of::<ZeCountRequest>() as u32,
        abi_reserved: 0,
        filter: std::ptr::null(),
        has_timestamp_range: 0,
        start_ts: 0,
        end_ts: 0,
    };
    let mut result: ZeCountResult = common::sized_zeroed();
    assert_eq!(ze_count(handle, &request, &mut result), ZeErrorCode::ZeOk);
    (result.count, result.generation)
}

fn open_read_only(path: &Path) -> ZeHandle {
    let bytes = path_bytes(path);
    let request = ZeOpenRequest {
        abi_size: size_of::<ZeOpenRequest>() as u32,
        abi_reserved: 0,
        path: bytes.as_ptr(),
        path_len: bytes.len(),
        access_mode: 1,
        durability_mode: 0,
        commit_tier: 1,
        reader_drain_timeout_ms: 250,
        max_resident_bytes: u64::MAX,
        max_temp_bytes: u64::MAX,
    };
    let mut handle = 0;
    assert_eq!(ze_open(&request, &mut handle), ZeErrorCode::ZeOk);
    handle
}

#[test]
fn snapshot_reports_its_generation_and_restores_as_a_store() {
    let mut store = common::TestStore::new();
    assert_eq!(common::ingest_rows(store.handle, 12, 8), ZeErrorCode::ZeOk);
    let (_, generation) = count(store.handle);
    let parent = tempfile::tempdir().expect("parent");
    let target = parent.path().join("backup");
    let bytes = path_bytes(&target);
    assert_eq!(
        snapshot(store.handle, &request(&bytes)),
        (ZeErrorCode::ZeOk, generation)
    );
    assert_eq!(common::ingest_rows(store.handle, 20, 8), ZeErrorCode::ZeOk);
    assert_eq!(store.close(), ZeErrorCode::ZeOk);

    let restored = open_read_only(&target);
    assert_eq!(count(restored).0, 12);
    assert_eq!(ze_close(restored), ZeErrorCode::ZeOk);
    let (code, writable) = common::open_path(&target);
    assert_eq!(code, ZeErrorCode::ZeOk);
    assert_eq!(count(writable).0, 12);
    assert_eq!(ze_close(writable), ZeErrorCode::ZeOk);
}

#[test]
fn snapshot_rejects_malformed_requests_without_writing() {
    let store = common::TestStore::new();
    assert_eq!(common::ingest_rows(store.handle, 2, 4), ZeErrorCode::ZeOk);
    let parent = tempfile::tempdir().expect("parent");
    let target = path_bytes(&parent.path().join("backup"));
    let invalid = ZeErrorCode::ZeErrInvalidArgument;

    let mut report: ZeGenerationReport = common::sized_zeroed();
    assert_eq!(
        ze_snapshot(store.handle, std::ptr::null(), &mut report),
        invalid
    );
    assert_eq!(
        ze_snapshot(store.handle, &request(&target), std::ptr::null_mut()),
        invalid
    );
    let mut undersized = request(&target);
    undersized.abi_size = 4;
    assert_eq!(snapshot(store.handle, &undersized).0, invalid);
    let mut reserved = request(&target);
    reserved.abi_reserved = 1;
    assert_eq!(snapshot(store.handle, &reserved).0, invalid);
    let mut dangling = request(&target);
    dangling.target = std::ptr::null();
    assert_eq!(snapshot(store.handle, &dangling).0, invalid);

    let cases: [(&[u8], &str); 3] = [
        (b"", "snapshot target must not be empty"),
        (b"back\0up", "path contains an interior NUL byte"),
        (b"back\xffup", "path is not valid UTF-8"),
    ];
    for (target, message) in cases {
        assert_eq!(snapshot(store.handle, &request(target)).0, invalid);
        assert_eq!(last_error(store.handle), message);
    }
    assert_eq!(
        snapshot(0, &request(&target)).0,
        ZeErrorCode::ZeErrInvalidHandle
    );
    assert!(
        std::fs::read_dir(parent.path())
            .expect("parent")
            .next()
            .is_none(),
        "no rejected request wrote anything"
    );
}

#[test]
fn snapshot_maps_target_and_access_mode_rejections() {
    let mut store = common::TestStore::new();
    assert_eq!(common::ingest_rows(store.handle, 2, 4), ZeErrorCode::ZeOk);
    let parent = tempfile::tempdir().expect("parent");
    let occupied = parent.path().join("occupied");
    std::fs::create_dir(&occupied).expect("occupied");
    std::fs::write(occupied.join("keep"), b"keep").expect("keep");
    let bytes = path_bytes(&occupied);
    assert_eq!(
        snapshot(store.handle, &request(&bytes)).0,
        ZeErrorCode::ZeErrInvalidArgument
    );
    assert_eq!(
        last_error(store.handle),
        format!("snapshot target {} is not empty", occupied.display())
    );
    let inside = path_bytes(&store.path.join("backup"));
    assert_eq!(
        snapshot(store.handle, &request(&inside)).0,
        ZeErrorCode::ZeErrInvalidArgument
    );
    assert!(last_error(store.handle).ends_with("is inside the store"));
    assert_eq!(store.close(), ZeErrorCode::ZeOk);

    let read_only = open_read_only(&store.path);
    let target = path_bytes(&parent.path().join("from-read-only"));
    assert_eq!(
        snapshot(read_only, &request(&target)).0,
        ZeErrorCode::ZeErrAccessMode
    );
    assert_eq!(ze_close(read_only), ZeErrorCode::ZeOk);
}
