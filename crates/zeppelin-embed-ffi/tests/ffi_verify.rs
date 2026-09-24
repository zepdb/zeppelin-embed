#![allow(clippy::expect_used, clippy::indexing_slicing, clippy::unwrap_used)]

mod common;

use std::mem::size_of;
use std::path::Path;

use zeppelin_embed::ingest::{DocId, DocumentVersion, IngestBatch, IngestDocument, Revision};
use zeppelin_embed::lifecycle::{OpenOptions, Store};
use zeppelin_embed_ffi::*;

const WAL_HEADER_LEN: usize = 40;

fn sealed_store(path: &Path) {
    let store = Store::open(path, OpenOptions::default()).expect("open");
    store
        .ingest(IngestBatch::new(
            (1..=4_u128)
                .map(|id| {
                    IngestDocument::new(
                        DocumentVersion::new(DocId::new(id), Revision::new(1)),
                        vec![id as f32, 1.0],
                    )
                    .with_text(format!("note {id}"))
                })
                .collect(),
        ))
        .expect("ingest");
    store.seal().expect("seal");
    // The seal truncates the WAL to its header (ZE-233); one upsert after
    // it leaves a record for the WAL checks to walk.
    store
        .ingest(IngestBatch::new(vec![
            IngestDocument::new(
                DocumentVersion::new(DocId::new(5), Revision::new(1)),
                vec![5.0, 1.0],
            )
            .with_text("note 5".to_owned()),
        ]))
        .expect("ingest WAL tail");
    store.close().expect("close");
}

fn request(path: &[u8]) -> ZeVerifyRequest {
    ZeVerifyRequest {
        abi_size: size_of::<ZeVerifyRequest>() as u32,
        abi_reserved: 0,
        path: path.as_ptr(),
        path_len: path.len(),
    }
}

fn path_bytes(path: &Path) -> Vec<u8> {
    path.to_string_lossy().into_owned().into_bytes()
}

fn last_error() -> String {
    let mut length = 0;
    assert_eq!(
        ze_last_error_message(0, std::ptr::null_mut(), 0, &mut length),
        ZeErrorCode::ZeOk
    );
    let mut buffer = vec![0_u8; length + 1];
    let mut written = 0;
    assert_eq!(
        ze_last_error_message(0, buffer.as_mut_ptr().cast(), buffer.len(), &mut written),
        ZeErrorCode::ZeOk
    );
    String::from_utf8_lossy(&buffer[..written]).into_owned()
}

#[test]
fn ze_verify_reports_a_clean_store_without_an_allocation() {
    let directory = tempfile::tempdir().expect("store");
    sealed_store(directory.path());
    let bytes = path_bytes(directory.path());
    let mut result: ZeVerifyResult = common::sized_zeroed();
    assert_eq!(ze_verify(&request(&bytes), &mut result), ZeErrorCode::ZeOk);
    assert_eq!(result.finding_count, 0);
    assert!(result.findings.is_null());
    assert_eq!(result.abi_reserved, 0);
    assert!(result.generation > 0);
    assert_eq!(result.segments_checked, 1);
    assert_eq!(result.wal_records_checked, 1);
    assert_eq!(ze_verify_result_free(&mut result), ZeErrorCode::ZeOk);
}

#[test]
fn ze_verify_returns_each_finding_with_its_file_offset_and_detail() {
    let directory = tempfile::tempdir().expect("store");
    sealed_store(directory.path());
    let wal = directory.path().join("wal.ze");
    let mut bytes = std::fs::read(&wal).expect("read WAL");
    bytes[WAL_HEADER_LEN + 20] ^= 0x5a;
    std::fs::write(&wal, bytes).expect("write WAL");
    let segment = std::fs::read_dir(directory.path())
        .expect("list")
        .map(|entry| entry.expect("entry").path())
        .find(|path| {
            path.extension()
                .is_some_and(|extension| extension == "zseg")
        })
        .expect("segment");
    std::fs::remove_file(&segment).expect("delete segment");

    let path = path_bytes(directory.path());
    let mut result: ZeVerifyResult = common::sized_zeroed();
    assert_eq!(ze_verify(&request(&path), &mut result), ZeErrorCode::ZeOk);
    assert_eq!(result.finding_count, 2);
    let findings = unsafe { std::slice::from_raw_parts(result.findings, result.finding_count) };
    let text = |pointer: *const u8, length: usize| {
        String::from_utf8(unsafe { std::slice::from_raw_parts(pointer, length) }.to_vec())
            .expect("UTF-8")
    };
    assert_eq!(findings[0].kind, ZE_VERIFY_SEGMENT_MISSING);
    assert_eq!(findings[0].has_offset, 0);
    assert_eq!(
        text(findings[0].file, findings[0].file_len),
        segment.file_name().unwrap().to_string_lossy()
    );
    assert_eq!(findings[1].kind, ZE_VERIFY_WAL_RECORD_CORRUPT);
    assert_eq!(findings[1].has_offset, 1);
    assert_eq!(findings[1].offset, WAL_HEADER_LEN as u64);
    assert_eq!(text(findings[1].file, findings[1].file_len), "wal.ze");
    assert!(!text(findings[1].detail, findings[1].detail_len).is_empty());
    assert_eq!(ze_verify_result_free(&mut result), ZeErrorCode::ZeOk);
    assert!(result.findings.is_null());
    assert_eq!(result.finding_count, 0);
    assert_eq!(ze_verify_result_free(&mut result), ZeErrorCode::ZeOk);
}

#[test]
fn ze_verify_rejects_invalid_requests_with_a_precise_error() {
    let directory = tempfile::tempdir().expect("root");
    let mut result: ZeVerifyResult = common::sized_zeroed();

    assert_eq!(
        ze_verify(std::ptr::null(), &mut result),
        ZeErrorCode::ZeErrInvalidArgument
    );
    let bytes = path_bytes(directory.path());
    assert_eq!(
        ze_verify(&request(&bytes), std::ptr::null_mut()),
        ZeErrorCode::ZeErrInvalidArgument
    );
    assert_eq!(
        ze_verify(&request(b""), &mut result),
        ZeErrorCode::ZeErrInvalidArgument
    );
    assert!(
        last_error().contains("must not be empty"),
        "{}",
        last_error()
    );
    assert_eq!(
        ze_verify(&request(b"store\0path"), &mut result),
        ZeErrorCode::ZeErrInvalidArgument
    );
    let mut reserved = request(&bytes);
    reserved.abi_reserved = 1;
    assert_eq!(
        ze_verify(&reserved, &mut result),
        ZeErrorCode::ZeErrInvalidArgument
    );
    assert!(last_error().contains("reserved"), "{}", last_error());

    let missing = path_bytes(&directory.path().join("absent"));
    assert_eq!(
        ze_verify(&request(&missing), &mut result),
        ZeErrorCode::ZeErrNotFound
    );
    assert!(!directory.path().join("absent").exists());
    let file = directory.path().join("file");
    std::fs::write(&file, b"x").expect("file");
    assert_eq!(
        ze_verify(&request(&path_bytes(&file)), &mut result),
        ZeErrorCode::ZeErrIo
    );
    assert!(last_error().contains("not a directory"), "{}", last_error());
    assert_eq!(result.finding_count, 0);

    assert_eq!(
        ze_verify_result_free(std::ptr::null_mut()),
        ZeErrorCode::ZeErrInvalidArgument
    );
    let mut forged: ZeVerifyResult = common::sized_zeroed();
    forged.finding_count = 1;
    assert_eq!(
        ze_verify_result_free(&mut forged),
        ZeErrorCode::ZeErrInvalidArgument
    );
}
