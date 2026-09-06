#![no_main]

//! Drives the C ABI with byte-derived requests. Every call must return a
//! typed status: a caught panic (`ZE_ERR_PANIC`) means boundary validation
//! let something through, and any memory fault is caught by the fuzzer's
//! sanitizer. Pointer/length pairs always describe real buffers, because a
//! caller lying about a length is UB by contract, exactly like `memcpy`.

use std::mem::size_of;
use std::sync::OnceLock;

use libfuzzer_sys::fuzz_target;
use zeppelin_embed_ffi::*;

const SCRATCH_DIMS: usize = 64;

struct Fixture {
    handle: ZeHandle,
    vector: Vec<f32>,
    path: Vec<u8>,
}

fn fixture() -> &'static Fixture {
    static FIXTURE: OnceLock<Fixture> = OnceLock::new();
    FIXTURE.get_or_init(|| {
        let path = std::env::temp_dir().join(format!(
            "zeppelin-embed-fuzz-ffi-{}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&path);
        let bytes = path.to_string_lossy().into_owned().into_bytes();
        let request = ZeOpenRequest {
            abi_size: size_of::<ZeOpenRequest>() as u32,
            abi_reserved: 0,
            path: bytes.as_ptr(),
            path_len: bytes.len(),
            access_mode: 0,
            durability_mode: 0,
            commit_tier: 1,
            reader_drain_timeout_ms: 10,
            max_resident_bytes: u64::MAX,
            max_temp_bytes: u64::MAX,
        };
        let mut handle = 0;
        assert_eq!(ze_open(&request, &mut handle), ZeErrorCode::ZeOk);
        let vector = (0..SCRATCH_DIMS)
            .map(|index| index as f32 / SCRATCH_DIMS as f32)
            .collect();
        Fixture {
            handle,
            vector,
            path: bytes,
        }
    })
}

struct Bytes<'a> {
    data: &'a [u8],
    cursor: usize,
}

impl<'a> Bytes<'a> {
    fn u8(&mut self) -> u8 {
        let value = self.data.get(self.cursor).copied().unwrap_or(0);
        self.cursor += 1;
        value
    }

    fn u32(&mut self) -> u32 {
        u32::from_le_bytes([self.u8(), self.u8(), self.u8(), self.u8()])
    }

    fn u64(&mut self) -> u64 {
        (u64::from(self.u32()) << 32) | u64::from(self.u32())
    }

    fn usize(&mut self) -> usize {
        self.u64() as usize
    }

    fn i32(&mut self) -> i32 {
        self.u32() as i32
    }

    /// A length that is honest about a buffer of `cap` elements.
    fn len_within(&mut self, cap: usize) -> usize {
        self.usize() % (cap + 1)
    }

    fn rest(&self) -> &'a [u8] {
        self.data.get(self.cursor..).unwrap_or(&[])
    }
}

fn sized<T>(abi_size: u32) -> T {
    let mut value = unsafe { std::mem::zeroed::<T>() };
    let pointer = (&mut value as *mut T).cast::<u32>();
    unsafe {
        std::ptr::write(pointer, abi_size);
    }
    value
}

fn abi_size<T>(bytes: &mut Bytes<'_>) -> u32 {
    match bytes.u8() % 8 {
        0 => bytes.u32(),
        1 => size_of::<T>() as u32 - 1,
        2 => size_of::<T>() as u32 + 64,
        _ => size_of::<T>() as u32,
    }
}

fn typed(code: ZeErrorCode) {
    let value = code as i32;
    assert!((0..=34).contains(&value), "unknown status code {value}");
    assert_ne!(
        code,
        ZeErrorCode::ZeErrPanic,
        "a panic crossed the boundary; validation is incomplete"
    );
}

fn tail_pointer(rest: &[u8], bytes: &mut Bytes<'_>) -> (*const u8, usize) {
    match bytes.u8() % 4 {
        0 => (std::ptr::null(), 0),
        1 => (std::ptr::null(), bytes.len_within(8)),
        _ => (rest.as_ptr(), bytes.len_within(rest.len())),
    }
}

fn tower(bytes: &mut Bytes<'_>, rest: &[u8]) -> ZeEmbeddingTower {
    let (model_id, model_id_len) = tail_pointer(rest, bytes);
    let (model_version, model_version_len) = tail_pointer(rest, bytes);
    let (weights_digest, weights_digest_len) = tail_pointer(rest, bytes);
    let (prompt_prefix, prompt_prefix_len) = tail_pointer(rest, bytes);
    let (os_build, os_build_len) = tail_pointer(rest, bytes);
    ZeEmbeddingTower {
        model_id,
        model_id_len,
        model_version,
        model_version_len,
        weights_digest,
        weights_digest_len,
        dims: bytes.u32(),
        normalization: bytes.i32() % 3,
        prompt_prefix,
        prompt_prefix_len,
        max_tokens: bytes.u32(),
        runtime: bytes.i32() % 5,
        compute_units: bytes.i32() % 6,
        has_os_build: bytes.u32() % 3,
        os_build,
        os_build_len,
    }
}

fuzz_target!(|data: &[u8]| {
    let fixture = fixture();
    let handle = if data.first().copied().unwrap_or(0) % 16 == 0 {
        // Occasionally aim at a never-valid or stale handle.
        u64::from(data.get(1).copied().unwrap_or(0))
    } else {
        fixture.handle
    };
    let mut bytes = Bytes { data, cursor: 2 };
    match bytes.u8() % 13 {
        0 => {
            let rest = bytes.rest();
            let dimension = bytes.len_within(SCRATCH_DIMS);
            let vector_len = bytes.len_within(SCRATCH_DIMS);
            let text_len = bytes.len_within(rest.len());
            let document = ZeIngestDocument {
                abi_size: abi_size::<ZeIngestDocument>(&mut bytes),
                abi_reserved: bytes.u32() % 2,
                doc_id: ZeDocId {
                    high: bytes.u64(),
                    low: bytes.u64(),
                },
                revision: bytes.u64(),
                timestamp: bytes.u64() as i64,
                vector: fixture.vector.as_ptr(),
                vector_len,
                metadata: rest.as_ptr(),
                metadata_len: bytes.len_within(rest.len()),
                text: rest.as_ptr(),
                text_len,
            };
            let documents = [document; 2];
            let request = ZeIngestRequest {
                abi_size: abi_size::<ZeIngestRequest>(&mut bytes),
                abi_reserved: 0,
                documents: documents.as_ptr(),
                document_count: bytes.len_within(documents.len()),
                dimension,
            };
            let mut report: ZeMutationReport = sized(size_of::<ZeMutationReport>() as u32);
            typed(ze_ingest(handle, &request, &mut report));
        }
        1 => {
            let ids = [
                ZeDocId {
                    high: bytes.u64(),
                    low: bytes.u64(),
                },
                ZeDocId { high: 0, low: 1 },
            ];
            let request = ZeDeleteRequest {
                abi_size: abi_size::<ZeDeleteRequest>(&mut bytes),
                abi_reserved: 0,
                doc_ids: ids.as_ptr(),
                doc_id_count: bytes.len_within(ids.len()),
            };
            let mut report: ZeMutationReport = sized(size_of::<ZeMutationReport>() as u32);
            typed(ze_delete(handle, &request, &mut report));
        }
        2 => {
            let request = ZeSearchRequest {
                abi_size: abi_size::<ZeSearchRequest>(&mut bytes),
                abi_reserved: 0,
                vector: fixture.vector.as_ptr(),
                vector_len: bytes.len_within(SCRATCH_DIMS),
                dimension: bytes.len_within(SCRATCH_DIMS),
                k: bytes.usize(),
                thread_budget: bytes.usize() % 4,
                has_tier: bytes.u32() % 3,
                tier: bytes.i32() % 6,
                graph_profile: bytes.i32() % 3,
                reserved: bytes.u32() % 2,
                graph_ef: bytes.usize() % 512,
                graph_seed: bytes.u64(),
                cancel_token: u64::from(bytes.u8() % 2),
                deadline_ns: u64::from(bytes.u8()) * 1_000_000,
            };
            let mut result: ZeSearchResult = sized(abi_size::<ZeSearchResult>(&mut bytes));
            let code = ze_search(handle, &request, &mut result);
            typed(code);
            typed(ze_search_result_free(&mut result));
        }
        3 => {
            let rest = bytes.rest();
            let (text, text_len) = tail_pointer(rest, &mut bytes);
            let request = ZeQueryRequest {
                abi_size: abi_size::<ZeQueryRequest>(&mut bytes),
                abi_reserved: 0,
                vector: fixture.vector.as_ptr(),
                vector_len: bytes.len_within(SCRATCH_DIMS),
                dimension: bytes.len_within(SCRATCH_DIMS),
                text,
                text_len,
                k: bytes.usize(),
                thread_budget: bytes.usize() % 4,
                has_tier: bytes.u32() % 3,
                tier: bytes.i32() % 6,
                graph_profile: bytes.i32() % 3,
                reserved: bytes.u32() % 2,
                graph_ef: bytes.usize() % 512,
                graph_seed: bytes.u64(),
                has_alpha: bytes.u32() % 3,
                rules_enabled: bytes.u32() % 3,
                alpha: f64::from_bits(bytes.u64()),
                has_max_rounds: bytes.u32() % 3,
                quoted_phrase: bytes.u32() % 3,
                max_rounds: bytes.u64() % 64,
                identifier_token: bytes.u32() % 3,
                has_rarest_exact_document_frequency: bytes.u32() % 3,
                rarest_exact_document_frequency: bytes.u64(),
                cancel_token: u64::from(bytes.u8() % 2),
                deadline_ns: u64::from(bytes.u8()) * 1_000_000,
            };
            let mut result: ZeQueryResult = sized(abi_size::<ZeQueryResult>(&mut bytes));
            typed(ze_query(handle, &request, &mut result));
            typed(ze_query_result_free(&mut result));
        }
        4 => {
            let rest = bytes.rest();
            let (alignment_digest, alignment_digest_len) = tail_pointer(rest, &mut bytes);
            let request = ZeEpochRequest {
                abi_size: abi_size::<ZeEpochRequest>(&mut bytes),
                abi_reserved: 0,
                embedding: ZeEmbeddingEpoch {
                    document: tower(&mut bytes, rest),
                    query: tower(&mut bytes, rest),
                    alignment_digest,
                    alignment_digest_len,
                },
                tokenizer_profile: bytes.i32() % 3,
                reserved: bytes.u32() % 2,
            };
            let mut identity: ZeEpochIdentity = sized(abi_size::<ZeEpochIdentity>(&mut bytes));
            typed(ze_epoch_identity(&request, &mut identity));
            let mut alias: ZeEpochAliasReport = sized(size_of::<ZeEpochAliasReport>() as u32);
            typed(ze_epoch_switch_alias(handle, &request, &mut alias));
            let mut drop: ZeEpochDropReport = sized(size_of::<ZeEpochDropReport>() as u32);
            typed(ze_epoch_drop(handle, &request, &mut drop));
            typed(ze_epoch_current(handle, &mut identity));
            let open = ZeOpenRequest {
                abi_size: abi_size::<ZeOpenRequest>(&mut bytes),
                abi_reserved: bytes.u32() % 2,
                path: fixture.path.as_ptr(),
                path_len: fixture.path.len(),
                access_mode: bytes.i32() % 4,
                durability_mode: bytes.i32() % 4,
                commit_tier: bytes.i32() % 4,
                reader_drain_timeout_ms: bytes.u64(),
                max_resident_bytes: bytes.u64(),
                max_temp_bytes: bytes.u64(),
            };
            let mut opened = 0;
            let code = ze_open(&open, &mut opened);
            typed(code);
            if code == ZeErrorCode::ZeOk {
                typed(ze_close(opened));
            }
            opened = 0;
            let code = ze_open_with_epoch(&open, &request, &mut opened);
            typed(code);
            if code == ZeErrorCode::ZeOk {
                typed(ze_close(opened));
            }
            assert!(ze_abi_version() > 0);
        }
        5 => {
            let request = ZeDropPartitionRequest {
                abi_size: abi_size::<ZeDropPartitionRequest>(&mut bytes),
                abi_reserved: 0,
                start_ts: bytes.u64() as i64,
                end_ts: bytes.u64() as i64,
            };
            let mut report: ZePartitionReport = sized(size_of::<ZePartitionReport>() as u32);
            typed(ze_drop_partition(handle, &request, &mut report));
            let retention = ZeRetentionRequest {
                abi_size: abi_size::<ZeRetentionRequest>(&mut bytes),
                abi_reserved: 0,
                window: bytes.u64() as i64,
                now_ts: bytes.u64() as i64,
            };
            typed(ze_apply_retention(handle, &retention, &mut report));
        }
        6 => {
            let seal = ZeSealRequest {
                abi_size: abi_size::<ZeSealRequest>(&mut bytes),
                abi_reserved: bytes.u32() % 2,
                cancel_token: u64::from(bytes.u8() % 2),
            };
            let mut generation: ZeGenerationReport =
                sized(abi_size::<ZeGenerationReport>(&mut bytes));
            typed(ze_seal(handle, &seal, &mut generation));
            let maintain = ZeMaintainRequest {
                abi_size: abi_size::<ZeMaintainRequest>(&mut bytes),
                abi_reserved: bytes.u32() % 2,
                wall_time_ns: bytes.u64(),
                bytes: bytes.u64(),
            };
            let mut report: ZeMaintainReport = sized(abi_size::<ZeMaintainReport>(&mut bytes));
            typed(ze_maintain(handle, &maintain, &mut report));
        }
        7 => {
            let ids = [
                ZeDocId {
                    high: bytes.u64(),
                    low: bytes.u64(),
                },
                ZeDocId { high: 0, low: 1 },
            ];
            let purge = ZePurgeRequest {
                abi_size: abi_size::<ZePurgeRequest>(&mut bytes),
                abi_reserved: bytes.u32() % 2,
                doc_ids: ids.as_ptr(),
                doc_id_count: bytes.len_within(ids.len()),
            };
            let mut token: ZePurgeTokenReport = sized(abi_size::<ZePurgeTokenReport>(&mut bytes));
            typed(ze_purge(handle, &purge, &mut token));
            let await_request = ZeAwaitPurgeRequest {
                abi_size: abi_size::<ZeAwaitPurgeRequest>(&mut bytes),
                abi_reserved: bytes.u32() % 2,
                token_id: if token.token_id == 0 {
                    bytes.u64()
                } else {
                    token.token_id
                },
            };
            let mut report: ZePurgeReport = sized(abi_size::<ZePurgeReport>(&mut bytes));
            typed(ze_await_physical_purge(handle, &await_request, &mut report));
        }
        8 => {
            let mut token = 0;
            let created = ze_cancel_token_create(&mut token);
            typed(created);
            if created == ZeErrorCode::ZeOk {
                typed(ze_cancel_token_cancel(token));
                typed(ze_cancel_token_cancel(token));
                typed(ze_cancel_token_free(token));
                typed(ze_cancel_token_free(token));
            }
            typed(ze_cancel_token_cancel(bytes.u64()));
        }
        9 => {
            let mut search: ZeSearchResult = sized(abi_size::<ZeSearchResult>(&mut bytes));
            typed(ze_search_result_free(&mut search));
            let mut query: ZeQueryResult = sized(abi_size::<ZeQueryResult>(&mut bytes));
            typed(ze_query_result_free(&mut query));
        }
        10 => {
            let mut buffer = vec![0_u8; bytes.len_within(64)];
            let mut written = 0;
            let pointer = if buffer.is_empty() {
                std::ptr::null_mut()
            } else {
                buffer.as_mut_ptr().cast()
            };
            typed(ze_last_error_message(handle, pointer, buffer.len(), &mut written));
            let _ = ze_error_code_name(bytes.i32());
            let mut state: ZeStateReport = sized(abi_size::<ZeStateReport>(&mut bytes));
            typed(ze_state(handle, &mut state));
            let mut stats: ZeStatsReport = sized(abi_size::<ZeStatsReport>(&mut bytes));
            typed(ze_stats(handle, &mut stats));
        }
        11 => {
            let rest = bytes.rest();
            let (name, name_len) = tail_pointer(rest, &mut bytes);
            let (attribute_name, attribute_name_len) = tail_pointer(rest, &mut bytes);
            let attribute = ZeAttributeDefinition {
                attribute_id: bytes.u32(),
                name: attribute_name,
                name_len: attribute_name_len,
                attribute_type: bytes.i32() % 8,
                nullable: bytes.u32() % 3,
            };
            let attributes = [attribute; 2];
            let spec = ZeNamespaceSpec {
                abi_size: abi_size::<ZeNamespaceSpec>(&mut bytes),
                abi_reserved: bytes.u32() % 2,
                attributes: attributes.as_ptr(),
                attribute_count: bytes.len_within(attributes.len()),
                has_vector_space: bytes.u32() % 3,
                dimensions: bytes.u32() % 128,
                normalization: bytes.i32() % 3,
                epoch: std::ptr::null(),
            };
            let open = ZeOpenRequest {
                abi_size: abi_size::<ZeOpenRequest>(&mut bytes),
                abi_reserved: bytes.u32() % 2,
                path: fixture.path.as_ptr(),
                path_len: fixture.path.len(),
                access_mode: bytes.i32() % 4,
                durability_mode: bytes.i32() % 4,
                commit_tier: bytes.i32() % 4,
                reader_drain_timeout_ms: bytes.u64(),
                max_resident_bytes: bytes.u64(),
                max_temp_bytes: bytes.u64(),
            };
            let namespace = ZeNamespaceOpenRequest {
                abi_size: abi_size::<ZeNamespaceOpenRequest>(&mut bytes),
                abi_reserved: bytes.u32() % 2,
                root: fixture.path.as_ptr(),
                root_len: fixture.path.len(),
                name,
                name_len,
                open,
                spec: &spec,
            };
            let mut opened = 0;
            let code = ze_namespace_open(&namespace, &mut opened);
            typed(code);
            if code == ZeErrorCode::ZeOk {
                typed(ze_close(opened));
            }
            let list = ZeNamespaceListRequest {
                abi_size: abi_size::<ZeNamespaceListRequest>(&mut bytes),
                abi_reserved: bytes.u32() % 2,
                root: fixture.path.as_ptr(),
                root_len: fixture.path.len(),
            };
            let mut result: ZeNamespaceListResult =
                sized(abi_size::<ZeNamespaceListResult>(&mut bytes));
            typed(ze_namespace_list(&list, &mut result));
            typed(ze_namespace_list_result_free(&mut result));
        }
        _ => typed(ze_close(u64::MAX.saturating_sub(bytes.u64() % 1_024))),
    }
});
