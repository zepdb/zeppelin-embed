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
        assert_eq!(code, ZeErrorCode::ZeOk, "open store");
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
            text: std::ptr::null(),
            text_len: 0,
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

pub fn valid_query_request(vector: &[f32]) -> ZeQueryRequest {
    ZeQueryRequest {
        abi_size: size_of::<ZeQueryRequest>() as u32,
        abi_reserved: 0,
        vector: vector.as_ptr(),
        vector_len: vector.len(),
        dimension: vector.len(),
        text: std::ptr::null(),
        text_len: 0,
        k: 1,
        thread_budget: 1,
        has_tier: 0,
        tier: 0,
        graph_profile: 0,
        reserved: 0,
        graph_ef: 0,
        graph_seed: 0,
        has_alpha: 0,
        rules_enabled: 0,
        alpha: 0.0,
        has_max_rounds: 0,
        quoted_phrase: 0,
        max_rounds: 0,
        identifier_token: 0,
        has_rarest_exact_document_frequency: 0,
        rarest_exact_document_frequency: 0,
        cancel_token: 0,
        deadline_ns: 0,
    }
}

/// Owns the byte buffers behind one `ZeEpochRequest`.
pub struct EpochFixture {
    pub model_id: Vec<u8>,
    pub model_version: Vec<u8>,
    pub weights_digest: Vec<u8>,
    pub document_prefix: Vec<u8>,
    pub query_prefix: Vec<u8>,
    pub alignment_digest: Vec<u8>,
    pub dims: u32,
}

impl EpochFixture {
    pub fn new(dims: u32) -> Self {
        Self {
            model_id: b"ffi-embedding".to_vec(),
            model_version: b"1".to_vec(),
            weights_digest: vec![0xad, 0x12],
            document_prefix: b"search_document: ".to_vec(),
            query_prefix: b"search_query: ".to_vec(),
            alignment_digest: Vec::new(),
            dims,
        }
    }

    pub fn core(&self) -> zeppelin_embed::epoch::StoreEpoch {
        use zeppelin_embed::epoch::*;
        let document = EmbeddingTower {
            model_id: String::from_utf8(self.model_id.clone()).expect("UTF-8 model id"),
            model_version: String::from_utf8(self.model_version.clone())
                .expect("UTF-8 model version"),
            weights_digest: self.weights_digest.clone(),
            dims: self.dims,
            normalization: Normalization::None,
            prompt_prefix: String::from_utf8(self.document_prefix.clone()).expect("UTF-8 prefix"),
            max_tokens: 64,
            runtime: EmbeddingRuntime::CpuReference,
            compute_units: ComputeUnits::Cpu,
            os_build: None,
        };
        let mut query = document.clone();
        query.prompt_prefix = String::from_utf8(self.query_prefix.clone()).expect("UTF-8 prefix");
        StoreEpoch {
            embedding: EmbeddingEpoch {
                document,
                query,
                alignment_digest: self.alignment_digest.clone(),
            },
            tokenizer: zeppelin_embed::fts::tokenizer::TokenizerConfig::text_default().epoch(),
        }
    }

    fn tower(&self, prefix: &[u8]) -> ZeEmbeddingTower {
        ZeEmbeddingTower {
            model_id: self.model_id.as_ptr(),
            model_id_len: self.model_id.len(),
            model_version: self.model_version.as_ptr(),
            model_version_len: self.model_version.len(),
            weights_digest: self.weights_digest.as_ptr(),
            weights_digest_len: self.weights_digest.len(),
            dims: self.dims,
            normalization: 0,
            prompt_prefix: prefix.as_ptr(),
            prompt_prefix_len: prefix.len(),
            max_tokens: 64,
            runtime: 3,
            compute_units: 1,
            has_os_build: 0,
            os_build: std::ptr::null(),
            os_build_len: 0,
        }
    }

    pub fn request(&self) -> ZeEpochRequest {
        ZeEpochRequest {
            abi_size: size_of::<ZeEpochRequest>() as u32,
            abi_reserved: 0,
            embedding: ZeEmbeddingEpoch {
                document: self.tower(&self.document_prefix),
                query: self.tower(&self.query_prefix),
                alignment_digest: self.alignment_digest.as_ptr(),
                alignment_digest_len: self.alignment_digest.len(),
            },
            tokenizer_profile: 0,
            reserved: 0,
        }
    }
}

pub fn open_path_with_epoch(path: &Path, epoch: &EpochFixture) -> (ZeErrorCode, ZeHandle) {
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
    let epoch = epoch.request();
    let mut handle = 0;
    let code = ze_open_with_epoch(&request, &epoch, &mut handle);
    (code, handle)
}
