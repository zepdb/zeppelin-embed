//! ZE-505 real-model fixture shared by contract and ownership tests.
use std::mem::size_of;
use zeppelin_embed_ffi::*;

pub struct TextStore {
    pub handle: ZeHandle,
    _directory: tempfile::TempDir,
}

impl TextStore {
    pub fn new() -> Self {
        let directory = tempfile::tempdir().expect("text fixture");
        let model = directory.path().join("fixture.zem");
        std::fs::write(
            &model,
            include_bytes!("../../../zeppelin-embed-text/tests/fixtures/ze115/bert.zem"),
        )
        .expect("text model fixture");
        Self::with_bundle(directory, &model)
    }

    pub fn with_bundle(directory: tempfile::TempDir, model: &std::path::Path) -> Self {
        let path = directory
            .path()
            .join("store")
            .to_string_lossy()
            .into_owned()
            .into_bytes();
        let model = model.to_string_lossy().into_owned().into_bytes();
        let request = ZeTextOpenRequest {
            abi_size: size_of::<ZeTextOpenRequest>() as u32,
            abi_reserved: 0,
            store: ZeOpenRequest {
                abi_size: size_of::<ZeOpenRequest>() as u32,
                abi_reserved: 0,
                path: path.as_ptr(),
                path_len: path.len(),
                access_mode: 0,
                durability_mode: 0,
                commit_tier: 1,
                reader_drain_timeout_ms: 100,
                max_resident_bytes: u64::MAX,
                max_temp_bytes: u64::MAX,
            },
            bundle_path: model.as_ptr(),
            bundle_path_len: model.len(),
        };
        let mut handle = 0;
        assert_eq!(ze_text_open(&request, &mut handle), ZeErrorCode::ZeOk);
        Self {
            handle,
            _directory: directory,
        }
    }
}

impl Drop for TextStore {
    fn drop(&mut self) {
        assert_eq!(ze_close(self.handle), ZeErrorCode::ZeOk);
    }
}

pub fn ingest_request(documents: &[ZeTextDocument]) -> ZeTextIngestRequestV2 {
    ZeTextIngestRequestV2 {
        abi_size: size_of::<ZeTextIngestRequestV2>() as u32,
        abi_reserved: 0,
        batch: ZeTextIngestRequest {
            abi_size: size_of::<ZeTextIngestRequest>() as u32,
            abi_reserved: 0,
            documents: documents.as_ptr(),
            document_count: documents.len(),
            embed_batch_size: 2,
            seal_every: 8,
            channel_capacity: 1,
        },
        chunk_policy: 1,
        reserved: 0,
        chunk_max: 3,
        chunk_overlap: 1,
        maintenance_wall_time_ns: 0,
        maintenance_bytes: 0,
        cancel_token: 0,
        deadline_ns: 0,
    }
}

pub fn query_request(text: &[u8]) -> ZeTextQueryRequestV2 {
    ZeTextQueryRequestV2 {
        abi_size: size_of::<ZeTextQueryRequestV2>() as u32,
        abi_reserved: 0,
        query: ZeTextQueryRequest {
            abi_size: size_of::<ZeTextQueryRequest>() as u32,
            abi_reserved: 0,
            text: text.as_ptr(),
            text_len: text.len(),
            k: 10,
            legs: 1,
            reserved: 0,
        },
        has_tier: 0,
        tier: 0,
        graph_profile: 0,
        scan_rescore: 0,
        graph_ef: 0,
        graph_seed: 0,
        oversample: 0,
        max_candidates_per_segment: 0,
        cancel_token: 0,
        deadline_ns: 0,
    }
}

pub fn output<T>() -> T {
    super::sized_zeroed()
}

pub fn document(id: u64, text: &[u8]) -> ZeTextDocument {
    ZeTextDocument {
        abi_size: size_of::<ZeTextDocument>() as u32,
        abi_reserved: 0,
        doc_id: ZeDocId { high: 0, low: id },
        revision: 1,
        text: text.as_ptr(),
        text_len: text.len(),
    }
}
