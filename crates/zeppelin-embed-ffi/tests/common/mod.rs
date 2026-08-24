#![allow(dead_code)]

use std::mem::size_of;
use std::path::{Path, PathBuf};

use tempfile::TempDir;
use zeppelin_embed_ffi::*;

pub fn sized_zeroed<T>() -> T {
    let mut value = unsafe { std::mem::zeroed::<T>() };
    let pointer = (&mut value as *mut T).cast::<u32>();
    unsafe {
        std::ptr::write(pointer, size_of::<T>() as u32);
        std::ptr::write(pointer.add(1), 0);
    }
    value
}

pub fn open_path(path: &Path) -> (ZeErrorCode, ZeHandle) {
    let bytes = path.to_string_lossy().into_owned().into_bytes();
    let request = ZeOpenRequest {
        abi_size: size_of::<ZeOpenRequest>() as u32,
        abi_reserved: 0,
        path: bytes.as_ptr(),
        path_len: bytes.len(),
        access_mode: 0,
        durability_mode: 0,
        commit_tier: 1,
        reader_drain_timeout_ms: 250,
        max_resident_bytes: u64::MAX,
        max_temp_bytes: u64::MAX,
    };
    let mut handle = 0;
    let code = ze_open(&request, &mut handle);
    (code, handle)
}

pub struct TestStore {
    pub handle: ZeHandle,
    pub path: PathBuf,
    directory: Option<TempDir>,
}

impl TestStore {
    pub fn new() -> Self {
        let directory = tempfile::tempdir().expect("temporary store directory");
        let path = directory.path().join("store");
        let (code, handle) = open_path(&path);
        assert_eq!(code, ZeErrorCode::Ok, "open store");
        Self {
            handle,
            path,
            directory: Some(directory),
        }
    }

    pub fn close(&mut self) -> ZeErrorCode {
        let code = ze_close(self.handle);
        self.handle = 0;
        code
    }
}

impl Drop for TestStore {
    fn drop(&mut self) {
        if self.handle != 0 {
            let _ = ze_close(self.handle);
        }
        let _ = self.directory.take();
    }
}

pub fn ingest_rows(handle: ZeHandle, rows: usize, dimension: usize) -> ZeErrorCode {
    let vector = (0..dimension)
        .map(|index| (index as f32 + 1.0) / dimension as f32)
        .collect::<Vec<_>>();
    let documents = (0..rows)
        .map(|index| ZeIngestDocument {
            abi_size: size_of::<ZeIngestDocument>() as u32,
            abi_reserved: 0,
            doc_id: ZeDocId {
                high: 0,
                low: index as u64 + 1,
            },
            revision: 1,
            timestamp: index as i64,
            vector: vector.as_ptr(),
            vector_len: vector.len(),
            metadata: std::ptr::null(),
            metadata_len: 0,
        })
        .collect::<Vec<_>>();
    let request = ZeIngestRequest {
        abi_size: size_of::<ZeIngestRequest>() as u32,
        abi_reserved: 0,
        documents: documents.as_ptr(),
        document_count: documents.len(),
        dimension,
    };
    let mut report: ZeMutationReport = sized_zeroed();
    ze_ingest(handle, &request, &mut report)
}

pub fn valid_search_request(vector: &[f32]) -> ZeSearchRequest {
    ZeSearchRequest {
        abi_size: size_of::<ZeSearchRequest>() as u32,
        abi_reserved: 0,
        vector: vector.as_ptr(),
        vector_len: vector.len(),
        dimension: vector.len(),
        k: 1,
        thread_budget: 1,
        search_tier: 1,
        graph_profile: 0,
        graph_ef: 0,
        graph_seed: 0,
        cancel_token: 0,
        deadline_ns: 0,
    }
}
