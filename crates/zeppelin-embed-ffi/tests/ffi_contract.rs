mod common;

use std::mem::{align_of, offset_of, size_of};
use std::process::Command;

use zeppelin_embed_ffi::*;

fn process_sensitive_lock() -> std::sync::MutexGuard<'static, ()> {
    static LOCK: std::sync::OnceLock<std::sync::Mutex<()>> = std::sync::OnceLock::new();
    LOCK.get_or_init(|| std::sync::Mutex::new(()))
        .lock()
        .expect("process-sensitive test mutex")
}

/// Frozen `(variant, value, C name)` triples. Append only: never renumber,
/// reuse, or remove a row.
const ERROR_CODE_GOLDEN: &[(ZeErrorCode, i32, &str)] = &[
    (ZeErrorCode::ZeOk, 0, "ZE_OK"),
    (
        ZeErrorCode::ZeErrInvalidArgument,
        1,
        "ZE_ERR_INVALID_ARGUMENT",
    ),
    (ZeErrorCode::ZeErrInvalidHandle, 2, "ZE_ERR_INVALID_HANDLE"),
    (ZeErrorCode::ZeErrClosed, 3, "ZE_ERR_CLOSED"),
    (ZeErrorCode::ZeErrClosing, 4, "ZE_ERR_CLOSING"),
    (ZeErrorCode::ZeErrPoisoned, 5, "ZE_ERR_POISONED"),
    (ZeErrorCode::ZeErrPanic, 6, "ZE_ERR_PANIC"),
    (ZeErrorCode::ZeErrBusy, 7, "ZE_ERR_BUSY"),
    (ZeErrorCode::ZeErrStoreBusy, 8, "ZE_ERR_STORE_BUSY"),
    (ZeErrorCode::ZeErrIo, 9, "ZE_ERR_IO"),
    (ZeErrorCode::ZeErrCorrupt, 10, "ZE_ERR_CORRUPT"),
    (ZeErrorCode::ZeErrUnsupported, 11, "ZE_ERR_UNSUPPORTED"),
    (ZeErrorCode::ZeErrCancelled, 12, "ZE_ERR_CANCELLED"),
    (ZeErrorCode::ZeErrTimeout, 13, "ZE_ERR_TIMEOUT"),
    (ZeErrorCode::ZeErrOutOfMemory, 14, "ZE_ERR_OUT_OF_MEMORY"),
    (
        ZeErrorCode::ZeErrBudgetExceeded,
        15,
        "ZE_ERR_BUDGET_EXCEEDED",
    ),
    (ZeErrorCode::ZeErrEmptyBatch, 16, "ZE_ERR_EMPTY_BATCH"),
    (ZeErrorCode::ZeErrStaleRevision, 17, "ZE_ERR_STALE_REVISION"),
    (
        ZeErrorCode::ZeErrDimensionMismatch,
        18,
        "ZE_ERR_DIMENSION_MISMATCH",
    ),
    (ZeErrorCode::ZeErrNotFound, 19, "ZE_ERR_NOT_FOUND"),
    (
        ZeErrorCode::ZeErrSynchronization,
        20,
        "ZE_ERR_SYNCHRONIZATION",
    ),
    (ZeErrorCode::ZeErrAccessMode, 21, "ZE_ERR_ACCESS_MODE"),
    (ZeErrorCode::ZeErrInternal, 22, "ZE_ERR_INTERNAL"),
    (ZeErrorCode::ZeErrEpochMismatch, 23, "ZE_ERR_EPOCH_MISMATCH"),
    (
        ZeErrorCode::ZeErrEpochUndeclared,
        24,
        "ZE_ERR_EPOCH_UNDECLARED",
    ),
    (
        ZeErrorCode::ZeErrEpochUnstamped,
        25,
        "ZE_ERR_EPOCH_UNSTAMPED",
    ),
    (
        ZeErrorCode::ZeErrEpochIncomplete,
        26,
        "ZE_ERR_EPOCH_INCOMPLETE",
    ),
    (
        ZeErrorCode::ZeErrEpochPublished,
        27,
        "ZE_ERR_EPOCH_PUBLISHED",
    ),
    (
        ZeErrorCode::ZeErrUnsealedWrites,
        28,
        "ZE_ERR_UNSEALED_WRITES",
    ),
    (ZeErrorCode::ZeErrBundle, 29, "ZE_ERR_BUNDLE"),
    (ZeErrorCode::ZeErrModel, 30, "ZE_ERR_MODEL"),
    (ZeErrorCode::ZeErrPipeline, 31, "ZE_ERR_PIPELINE"),
    (ZeErrorCode::ZeErrScanStale, 32, "ZE_ERR_SCAN_STALE"),
    (
        ZeErrorCode::ZeErrSchemaMismatch,
        33,
        "ZE_ERR_SCHEMA_MISMATCH",
    ),
    (
        ZeErrorCode::ZeErrNoVectorSpace,
        34,
        "ZE_ERR_NO_VECTOR_SPACE",
    ),
    (ZeErrorCode::ZeErrStoreKind, 35, "ZE_ERR_STORE_KIND"),
    (ZeErrorCode::ZeErrFormatVersion, 36, "ZE_ERR_FORMAT_VERSION"),
    (ZeErrorCode::ZeErrQuerySyntax, 37, "ZE_ERR_QUERY_SYNTAX"),
    (
        ZeErrorCode::ZeErrQueryUnsupported,
        38,
        "ZE_ERR_QUERY_UNSUPPORTED",
    ),
    (ZeErrorCode::ZeErrParameter, 39, "ZE_ERR_PARAMETER"),
    (ZeErrorCode::ZeErrType, 40, "ZE_ERR_TYPE"),
    (ZeErrorCode::ZeErrScope, 41, "ZE_ERR_SCOPE"),
    (ZeErrorCode::ZeErrKeyConflict, 42, "ZE_ERR_KEY_CONFLICT"),
    (
        ZeErrorCode::ZeErrIncarnationConflict,
        43,
        "ZE_ERR_INCARNATION_CONFLICT",
    ),
    (
        ZeErrorCode::ZeErrDeletionRevisionConflict,
        44,
        "ZE_ERR_DELETION_REVISION_CONFLICT",
    ),
    (ZeErrorCode::ZeErrEndpoint, 45, "ZE_ERR_ENDPOINT"),
    (ZeErrorCode::ZeErrDeletedEntity, 46, "ZE_ERR_DELETED_ENTITY"),
    (
        ZeErrorCode::ZeErrArithmeticDomain,
        47,
        "ZE_ERR_ARITHMETIC_DOMAIN",
    ),
    (
        ZeErrorCode::ZeErrArithmeticOverflow,
        48,
        "ZE_ERR_ARITHMETIC_OVERFLOW",
    ),
    (
        ZeErrorCode::ZeErrDivisionByZero,
        49,
        "ZE_ERR_DIVISION_BY_ZERO",
    ),
    (
        ZeErrorCode::ZeErrIndeterminateCommit,
        50,
        "ZE_ERR_INDETERMINATE_COMMIT",
    ),
    (
        ZeErrorCode::ZeErrRevisionOverflow,
        51,
        "ZE_ERR_REVISION_OVERFLOW",
    ),
    (
        ZeErrorCode::ZeErrGenerationOverflow,
        52,
        "ZE_ERR_GENERATION_OVERFLOW",
    ),
    (
        ZeErrorCode::ZeErrDuplicateTarget,
        53,
        "ZE_ERR_DUPLICATE_TARGET",
    ),
    (
        ZeErrorCode::ZeErrIdentityOverflow,
        54,
        "ZE_ERR_IDENTITY_OVERFLOW",
    ),
    (
        ZeErrorCode::ZeErrRevisionConflict,
        55,
        "ZE_ERR_REVISION_CONFLICT",
    ),
    (ZeErrorCode::ZeErrFormatTooNew, 56, "ZE_ERR_FORMAT_TOO_NEW"),
    (ZeErrorCode::ZeErrCascadeCycle, 57, "ZE_ERR_CASCADE_CYCLE"),
];

fn header_error_codes() -> Vec<(String, i32)> {
    let header = std::fs::read_to_string(
        std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("include/zeppelin_embed.h"),
    )
    .expect("committed C header");
    let start = header.find("enum ze_error_code").expect("error-code enum");
    let body = &header[start..];
    let end = body.find("};").expect("error-code enum end");
    body[..end]
        .lines()
        .filter_map(|line| {
            let line = line.trim().trim_end_matches(',');
            let (name, value) = line.split_once(" = ")?;
            name.starts_with("ZE_")
                .then(|| (name.to_owned(), value.parse().expect("enumerator value")))
        })
        .collect()
}

#[test]
fn error_codes_are_append_only() {
    // Three copies of the table exist: the Rust enum, the C enumerators in
    // the committed header, and the `ze_error_code_name` strings. Each row
    // must agree in all three, values must stay contiguous from zero, and
    // the first unassigned value must still be unknown.
    let mut expected_header = Vec::new();
    for (index, (code, value, name)) in ERROR_CODE_GOLDEN.iter().enumerate() {
        assert_eq!(*code as i32, *value, "{name} numeric value");
        assert_eq!(*value, index as i32, "{name} is not contiguous");
        let actual = unsafe { std::ffi::CStr::from_ptr(ze_error_code_name(*value)) };
        assert_eq!(actual.to_str(), Ok(*name), "ze_error_code_name({value})");
        expected_header.push(((*name).to_owned(), *value));
    }
    assert_eq!(
        header_error_codes(),
        expected_header,
        "C header enumerators"
    );
    let next = ERROR_CODE_GOLDEN.len() as i32;
    let unknown = unsafe { std::ffi::CStr::from_ptr(ze_error_code_name(next)) };
    assert_eq!(unknown.to_str(), Ok("ZE_ERR_UNKNOWN"));
}

fn swift_error_case(c_name: &str) -> String {
    let stem = c_name
        .strip_prefix("ZE_ERR_")
        .or_else(|| c_name.strip_prefix("ZE_"))
        .expect("error-code C prefix");
    let mut words = stem.split('_');
    let mut name = words
        .next()
        .expect("error-code name word")
        .to_ascii_lowercase();
    for word in words {
        let mut characters = word.chars();
        if let Some(first) = characters.next() {
            name.push(first.to_ascii_uppercase());
            name.extend(characters.map(|character| character.to_ascii_lowercase()));
        }
    }
    if name == "internal" {
        name.push_str("Error");
    }
    name
}

fn generated_swift_error_enum() -> String {
    use std::fmt::Write as _;

    let mut source = String::from(
        "// Generated from ffi_contract::ERROR_CODE_GOLDEN. Do not edit by hand.\n\n\
         public enum ZeppelinError: Int32, Error, Sendable, CaseIterable {\n",
    );
    for (_, value, c_name) in ERROR_CODE_GOLDEN {
        writeln!(source, "    case {} = {value}", swift_error_case(c_name))
            .expect("write generated Swift error case");
    }
    source.push_str("}\n");
    source
}

#[test]
fn swift_error_enum_is_generated_from_the_append_only_error_table() {
    let workspace = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .and_then(std::path::Path::parent)
        .expect("workspace root");
    let path = workspace.join("bindings/swift/Sources/ZeppelinEmbed/ZeppelinError.swift");
    let generated = generated_swift_error_enum();
    if std::env::var_os("ZE_WRITE_SWIFT_ERROR").as_deref() == Some(std::ffi::OsStr::new("1")) {
        std::fs::write(&path, &generated).expect("write generated Swift error enum");
    }
    let committed = std::fs::read_to_string(&path).expect("committed Swift error enum");
    assert_eq!(
        committed, generated,
        "Swift error enum drifted; regenerate with \
         ZE_WRITE_SWIFT_ERROR=1 cargo test -p zeppelin-embed-ffi --test ffi_contract \
         swift_error_enum_is_generated_from_the_append_only_error_table -- --exact"
    );
}

macro_rules! assert_layout {
    ($type:ty, $size:literal, $align:literal, {$($field:ident: $offset:literal),+ $(,)?}) => {{
        assert_eq!(size_of::<$type>(), $size, "{} size", stringify!($type));
        assert_eq!(align_of::<$type>(), $align, "{} alignment", stringify!($type));
        $(assert_eq!(offset_of!($type, $field), $offset,
            "{}.{} offset", stringify!($type), stringify!($field));)+
    }};
}

#[test]
fn every_request_struct_has_the_frozen_size_and_field_offsets() {
    assert_layout!(ZeOpenRequest, 64, 8, {
        abi_size: 0, abi_reserved: 4, path: 8, path_len: 16,
        access_mode: 24, durability_mode: 28, commit_tier: 32,
        reader_drain_timeout_ms: 40, max_resident_bytes: 48, max_temp_bytes: 56
    });
    assert_layout!(ZeStateReport, 16, 4, {
        abi_size: 0, abi_reserved: 4, state: 8, reserved: 12
    });
    assert_layout!(ZeStatsReport, 144, 8, {
        abi_size: 0, abi_reserved: 4, resident_owned_bytes: 8, mapped_bytes: 16,
        mapped_resident_bytes: 24, segment_bytes: 32, active_segment_bytes: 40,
        active_row_count: 48, tombstone_count: 56, tombstone_bytes: 64,
        wal_bytes: 72, cache_bytes: 80, temporary_bytes: 88,
        query_pool_bytes: 96, open_files: 104, active_queries: 112,
        active_snapshot_leases: 120, phys_footprint: 128,
        has_phys_footprint: 136, reserved: 140
    });
    assert_layout!(ZeDocId, 16, 8, { high: 0, low: 8 });
    assert_layout!(ZeIngestDocument, 88, 8, {
        abi_size: 0, abi_reserved: 4, doc_id: 8, revision: 24, timestamp: 32,
        vector: 40, vector_len: 48, metadata: 56, metadata_len: 64,
        text: 72, text_len: 80
    });
    assert_layout!(ZeIngestRequest, 32, 8, {
        abi_size: 0, abi_reserved: 4, documents: 8, document_count: 16, dimension: 24
    });
    assert_layout!(ZeDeleteRequest, 24, 8, {
        abi_size: 0, abi_reserved: 4, doc_ids: 8, doc_id_count: 16
    });
    assert_layout!(ZeMutationReport, 24, 8, {
        abi_size: 0, abi_reserved: 4, sequence: 8, generation: 16
    });
    assert_layout!(ZeDeleteWhereRequest, 16, 8, {
        abi_size: 0, abi_reserved: 4, filter: 8
    });
    assert_layout!(ZeDeleteWhereReport, 24, 8, {
        abi_size: 0, abi_reserved: 4, deleted_count: 8, generation: 16
    });
    assert_layout!(ZeSearchRequest, 96, 8, {
        abi_size: 0, abi_reserved: 4, vector: 8, vector_len: 16,
        dimension: 24, k: 32, thread_budget: 40, has_tier: 48, tier: 52,
        graph_profile: 56, reserved: 60, graph_ef: 64, graph_seed: 72,
        cancel_token: 80, deadline_ns: 88
    });
    assert_layout!(ZeSearchHit, 64, 8, {
        source_kind: 0, reserved: 4, segment_id: 8, local_row: 24,
        has_document: 28, doc_id: 32, revision: 48, score: 56, reserved_tail: 60
    });
    assert_layout!(ZeSearchResult, 112, 8, {
        abi_size: 0, abi_reserved: 4, hits: 8, hit_count: 16, generation: 24,
        dims_touched: 32, bytes_read: 40, threads_used: 48,
        graph_segments_traversed: 56, graph_validations: 64,
        graph_entry_seed_discoveries: 72, graph_visited_epoch_clears: 80,
        graph_candidates_scored: 88, graph_candidates_rescored: 96,
        graph_segments_pruned_by_bound: 104
    });
    assert_layout!(ZeSealRequest, 16, 8, {
        abi_size: 0, abi_reserved: 4, cancel_token: 8
    });
    assert_layout!(ZeGenerationReport, 16, 8, {
        abi_size: 0, abi_reserved: 4, generation: 8
    });
    assert_layout!(ZeSnapshotRequest, 24, 8, {
        abi_size: 0, abi_reserved: 4, target: 8, target_len: 16
    });
    assert_layout!(ZeDropPartitionRequest, 24, 8, {
        abi_size: 0, abi_reserved: 4, start_ts: 8, end_ts: 16
    });
    assert_layout!(ZeRetentionRequest, 24, 8, {
        abi_size: 0, abi_reserved: 4, window: 8, now_ts: 16
    });
    assert_layout!(ZePartitionReport, 48, 8, {
        abi_size: 0, abi_reserved: 4, generation: 8, segments_dropped: 16,
        bytes_reclaimed: 24, straddlers_skipped: 32, is_no_op: 40, reserved: 44
    });
    assert_layout!(ZePurgeRequest, 24, 8, {
        abi_size: 0, abi_reserved: 4, doc_ids: 8, doc_id_count: 16
    });
    assert_layout!(ZePurgeTokenReport, 40, 8, {
        abi_size: 0, abi_reserved: 4, token_id: 8, generation: 16,
        unknown_id_count: 24, is_no_op: 32, reserved: 36
    });
    assert_layout!(ZeAwaitPurgeRequest, 16, 8, {
        abi_size: 0, abi_reserved: 4, token_id: 8
    });
    assert_layout!(ZePurgeReport, 40, 8, {
        abi_size: 0, abi_reserved: 4, generation: 8, segments_rewritten: 16,
        unknown_id_count: 24, wal_rewritten: 32, is_no_op: 36
    });
    assert_layout!(ZeMaintainRequest, 24, 8, {
        abi_size: 0, abi_reserved: 4, wall_time_ns: 8, bytes: 16
    });
    assert_layout!(ZeMaintainReport, 40, 8, {
        abi_size: 0, abi_reserved: 4, graphs_built: 8, bytes_consumed: 16,
        checkpoints_resumed: 24, status: 32, reserved: 36
    });
}

#[test]
fn every_phase_two_struct_has_the_frozen_size_and_field_offsets() {
    assert_layout!(ZeAttributeDefinition, 32, 8, {
        attribute_id: 0, name: 8, name_len: 16, attribute_type: 24, nullable: 28
    });
    assert_layout!(ZeAttributeValue, 56, 8, {
        attribute_id: 0, value_type: 4, u64_value: 8, i64_value: 16,
        f64_value: 24, bool_value: 32, string_value: 40, string_len: 48
    });
    assert_layout!(ZeUpsertDocument, 112, 8, {
        abi_size: 0, abi_reserved: 4, document: 8, attributes: 96,
        attribute_count: 104
    });
    assert_layout!(ZeUpsertRequest, 32, 8, {
        abi_size: 0, abi_reserved: 4, documents: 8, document_count: 16,
        dimension: 24
    });
    assert_layout!(ZeRevisionCondition, 16, 8, {
        kind: 0, reserved: 4, revision: 8
    });
    assert_layout!(ZeConditionalUpsertRequest, 56, 8, {
        abi_size: 0, abi_reserved: 4, batch: 8, conditions: 40, condition_count: 48
    });
    assert_layout!(ZeConditionalDeleteRequest, 48, 8, {
        abi_size: 0, abi_reserved: 4, batch: 8, conditions: 32, condition_count: 40
    });
    assert_layout!(ZeRevisionConflict, 56, 8, {
        abi_size: 0, abi_reserved: 4, index: 8, doc_id: 16, expected_kind: 32,
        has_current: 36, expected_revision: 40, current_revision: 48
    });
    assert_layout!(ZeGetRequest, 40, 8, {
        abi_size: 0, abi_reserved: 4, ids: 8, id_count: 16,
        include_vector: 24, include_text: 28, include_metadata: 32,
        include_attributes: 36
    });
    assert_layout!(ZeStoredDocument, 104, 8, {
        has_document: 0, doc_id: 8, revision: 24, timestamp: 32,
        vector: 40, vector_len: 48, text: 56, text_len: 64,
        metadata: 72, metadata_len: 80, attributes: 88, attribute_count: 96
    });
    assert_layout!(ZeGetResult, 40, 8, {
        abi_size: 0, abi_reserved: 4, documents: 8, document_count: 16,
        missing_count: 24, generation: 32
    });
    assert_layout!(ZeFilterNode, 168, 8, {
        op: 0, attribute_id: 4, values: 8, value_count: 16,
        has_lower: 24, lower: 32, lower_inclusive: 88, has_upper: 92,
        upper: 96, upper_inclusive: 152, children_start: 156,
        children_count: 160
    });
    assert_layout!(ZeFilter, 32, 8, {
        abi_size: 0, abi_reserved: 4, nodes: 8, node_count: 16, root: 24
    });
    assert_layout!(ZeCountRequest, 40, 8, {
        abi_size: 0, abi_reserved: 4, filter: 8, has_timestamp_range: 16,
        start_ts: 24, end_ts: 32
    });
    assert_layout!(ZeCountResult, 24, 8, {
        abi_size: 0, abi_reserved: 4, count: 8, generation: 16
    });
    assert_layout!(ZeCountGroupedRequest, 64, 8, {
        abi_size: 0, abi_reserved: 4, count: 8, group_attribute_id: 48,
        reserved: 52, group_limit: 56
    });
    assert_layout!(ZeCountGroup, 64, 8, { value: 0, count: 56 });
    assert_layout!(ZeCountGroupedResult, 48, 8, {
        abi_size: 0, abi_reserved: 4, groups: 8, group_count: 16,
        missing_count: 24, count: 32, generation: 40
    });
    assert_layout!(ZeSearchFilteredRequest, 112, 8, {
        abi_size: 0, abi_reserved: 4, search: 8, filter: 104
    });
    assert_layout!(ZeScanRequest, 112, 8, {
        abi_size: 0, abi_reserved: 4, cursor_generation: 8,
        cursor_segment_id: 16, cursor_next_row: 32, cursor_phase: 36,
        limit: 40, order: 48, include_vector: 52, include_text: 56,
        include_metadata: 60, include_attributes: 64,
        has_timestamp_range: 68, start_ts: 72, end_ts: 80, filter: 88,
        cancel_token: 96, deadline_ns: 104
    });
    assert_layout!(ZeScanResult, 64, 8, {
        abi_size: 0, abi_reserved: 4, documents: 8, document_count: 16,
        generation: 24, has_more: 32, next_segment_id: 36, next_row: 52,
        next_phase: 56
    });
    assert_layout!(ZeScanOrderedRequest, 136, 8, {
        abi_size: 0, abi_reserved: 4, scan: 8, order_attribute_id: 120,
        cursor_order: 124, cursor_order_attribute_id: 128
    });
    assert_layout!(ZeNamespaceSpec, 48, 8, {
        abi_size: 0, abi_reserved: 4, attributes: 8, attribute_count: 16,
        has_vector_space: 24, dimensions: 28, normalization: 32, epoch: 40
    });
    assert_layout!(ZeNamespaceOpenRequest, 112, 8, {
        abi_size: 0, abi_reserved: 4, root: 8, root_len: 16, name: 24,
        name_len: 32, open: 40, spec: 104
    });
    assert_layout!(ZeNamespaceListRequest, 24, 8, {
        abi_size: 0, abi_reserved: 4, root: 8, root_len: 16
    });
    assert_layout!(ZeNamespaceEntry, 16, 8, { name: 0, name_len: 8 });
    assert_layout!(ZeNamespaceListResult, 24, 8, {
        abi_size: 0, abi_reserved: 4, entries: 8, entry_count: 16
    });
    assert_layout!(ZeVerifyRequest, 24, 8, {
        abi_size: 0, abi_reserved: 4, path: 8, path_len: 16
    });
    assert_layout!(ZeVerifyFinding, 48, 8, {
        kind: 0, has_offset: 4, offset: 8, file: 16, file_len: 24, detail: 32,
        detail_len: 40
    });
    assert_layout!(ZeVerifyResult, 48, 8, {
        abi_size: 0, abi_reserved: 4, findings: 8, finding_count: 16,
        generation: 24, segments_checked: 32, wal_records_checked: 40
    });
    assert_layout!(ZeEmbeddingTower, 104, 8, {
        model_id: 0, model_id_len: 8, model_version: 16, model_version_len: 24,
        weights_digest: 32, weights_digest_len: 40, dims: 48, normalization: 52,
        prompt_prefix: 56, prompt_prefix_len: 64, max_tokens: 72, runtime: 76,
        compute_units: 80, has_os_build: 84, os_build: 88, os_build_len: 96
    });
    assert_layout!(ZeEmbeddingEpoch, 224, 8, {
        document: 0, query: 104, alignment_digest: 208, alignment_digest_len: 216
    });
    assert_layout!(ZeEpochRequest, 240, 8, {
        abi_size: 0, abi_reserved: 4, embedding: 8, tokenizer_profile: 232, reserved: 236
    });
    assert_layout!(ZeEpochIdentity, 24, 8, {
        abi_size: 0, abi_reserved: 4, embedding_epoch: 8, tokenizer_epoch: 16
    });
    assert_layout!(ZeEpochAliasReport, 56, 8, {
        abi_size: 0, abi_reserved: 4, generation: 8, previous_embedding_epoch: 16,
        previous_tokenizer_epoch: 24, published_embedding_epoch: 32,
        published_tokenizer_epoch: 40, manifest_committed: 48, reserved: 52
    });
    assert_layout!(ZeEpochDropReport, 32, 8, {
        abi_size: 0, abi_reserved: 4, generation: 8, segments_dropped: 16,
        bytes_reclaimed: 24
    });
    assert_layout!(ZeQueryRequest, 160, 8, {
        abi_size: 0, abi_reserved: 4, vector: 8, vector_len: 16, dimension: 24,
        text: 32, text_len: 40, k: 48, thread_budget: 56, has_tier: 64, tier: 68,
        graph_profile: 72, lexical_flags: 76, graph_ef: 80, graph_seed: 88,
        has_alpha: 96, rules_enabled: 100, alpha: 104, has_max_rounds: 112,
        quoted_phrase: 116, max_rounds: 120, identifier_token: 128,
        has_rarest_exact_document_frequency: 132,
        rarest_exact_document_frequency: 136, cancel_token: 144, deadline_ns: 152
    });
    assert_layout!(ZeQueryFilter, 40, 8, {
        abi_size: 0, abi_reserved: 4, filter: 8, has_timestamp_range: 16,
        start_ts: 24, end_ts: 32
    });
    assert_layout!(ZeQueryHit, 64, 8, {
        has_document: 0, has_revision: 4, doc_id: 8, revision: 24, score: 32,
        has_vector_score: 40, has_lexical_score: 44, vector_squared_l2: 48,
        lexical_bm25: 56
    });
    assert_layout!(ZeQueryResult, 128, 8, {
        abi_size: 0, abi_reserved: 4, hits: 8, hit_count: 16, generation: 24,
        mode: 32, approximate: 36, exact_rescore: 40, budget_exhausted: 44,
        has_fusion: 48, fusion_method: 52, effective_alpha: 56, fusion_rounds: 64,
        has_embedding_epoch: 72, has_tokenizer_epoch: 76, embedding_epoch: 80,
        tokenizer_epoch: 88, dims_touched: 96, bytes_read: 104,
        docs_evaluated: 112, postings_decoded: 120
    });
    assert_layout!(ZeSnippetSourceRanges, 40, 8, {
        abi_size: 0, abi_reserved: 4, source_start: 8, source_end: 16,
        highlights: 24, highlight_count: 32
    });
    assert_layout!(ZeSnippetHighlight, 16, 8, { start: 0, end: 8 });
    assert_layout!(ZeQuerySnippet, 48, 8, {
        has_snippet: 0, truncated_start: 4, truncated_end: 8, reserved: 12,
        text: 16, text_len: 24, highlights: 32, highlight_count: 40
    });
    assert_layout!(ZeQuerySnippets, 24, 8, {
        abi_size: 0, abi_reserved: 4, snippets: 8, snippet_count: 16
    });
}

#[test]
fn use_after_close_double_close_and_a_stale_generation_each_return_typed_errors() {
    let _process_guard = process_sensitive_lock();
    let directory = tempfile::tempdir().expect("temporary directory");
    let path = directory.path().join("store");
    let (code, first) = common::open_path(&path);
    assert_eq!(code, ZeErrorCode::ZeOk);
    assert_ne!(first, 0);
    assert_eq!(ze_close(first), ZeErrorCode::ZeOk);

    let mut state: ZeStateReport = common::sized_zeroed();
    assert_eq!(ze_state(first, &mut state), ZeErrorCode::ZeErrClosed);
    assert_eq!(ze_close(first), ZeErrorCode::ZeErrClosed);

    let (code, second) = common::open_path(&path);
    assert_eq!(code, ZeErrorCode::ZeOk);
    assert_ne!(first, second, "slot reuse must bump the generation");
    assert_eq!(ze_state(first, &mut state), ZeErrorCode::ZeErrClosed);
    assert_eq!(ze_state(second, &mut state), ZeErrorCode::ZeOk);
    assert_eq!(ze_state(0, &mut state), ZeErrorCode::ZeErrInvalidHandle);
    assert_eq!(ze_close(second), ZeErrorCode::ZeOk);
}

const PANIC_PROBE: u32 = 0x5041_4e49;
const POISON_TABLE_NAMES: &[&str] = &[
    "ze_namespace_batch",
    "ze_namespace_declare_cascade",
    "ze_namespace_delete_cascade",
    "ze_apply_retention",
    "ze_await_physical_purge",
    "ze_close",
    "ze_count",
    "ze_count_grouped",
    "ze_delete",
    "ze_delete_conditional",
    "ze_delete_where",
    "ze_drop_partition",
    "ze_epoch_current",
    "ze_epoch_drop",
    "ze_epoch_switch_alias",
    "ze_get",
    "ze_ingest",
    "ze_maintain",
    "ze_purge",
    "ze_query",
    "ze_query_snippet_source_ranges",
    "ze_query_with_snippets",
    "ze_query_filtered",
    "ze_scan",
    "ze_scan_ordered",
    "ze_scan_result_free",
    "ze_schema_column",
    "ze_seal",
    "ze_merge_sealed",
    "ze_open_migrations",
    "ze_reindex_text",
    "ze_search",
    "ze_search_filtered",
    "ze_snapshot",
    "ze_open_snapshot",
    "ze_state",
    "ze_stats",
    "ze_upsert",
    "ze_upsert_conditional",
];

fn delegate_to_panic_feature(test_name: &str) -> bool {
    if cfg!(feature = "abi-panic-probe") {
        return false;
    }
    let workspace = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .and_then(std::path::Path::parent)
        .expect("workspace root");
    // A sanitizer build passes `-Zbuild-std --target <triple>`; the child
    // cargo must repeat them or its dependencies fail the sanitizer ABI
    // check. CI sets ZE_CARGO_TEST_ARGS for exactly that.
    let extra = std::env::var("ZE_CARGO_TEST_ARGS")
        .map(|value| {
            value
                .split_whitespace()
                .map(str::to_owned)
                .collect::<Vec<_>>()
        })
        .unwrap_or_default();
    let status = Command::new(std::env::var("CARGO").unwrap_or_else(|_| "cargo".to_owned()))
        .current_dir(workspace)
        .arg("test")
        .args(extra)
        .args([
            "-p",
            "zeppelin-embed-ffi",
            "--features",
            "abi-panic-probe",
            "--test",
            "ffi_contract",
            test_name,
            "--",
            "--exact",
            "--nocapture",
        ])
        .status()
        .expect("run panic-probe feature test");
    assert!(status.success(), "panic-probe feature child failed");
    true
}

fn trigger_panic(handle: ZeHandle) -> ZeErrorCode {
    let vector = [1.0_f32];
    let mut request = common::valid_search_request(&vector);
    request.abi_reserved = PANIC_PROBE;
    let mut result: ZeSearchResult = common::sized_zeroed();
    ze_search(handle, &request, &mut result)
}

#[cfg(feature = "abi-panic-probe")]
fn arm_named_panic(entry_point: &'static str) {
    zeppelin_embed_ffi::arm_abi_panic_probe(entry_point);
}

#[cfg(not(feature = "abi-panic-probe"))]
fn arm_named_panic(_entry_point: &'static str) {}

#[test]
fn a_panic_crossing_the_abi_is_caught_the_handle_is_poisoned_and_the_process_survives() {
    let _process_guard = process_sensitive_lock();
    const NAME: &str =
        "a_panic_crossing_the_abi_is_caught_the_handle_is_poisoned_and_the_process_survives";
    if delegate_to_panic_feature(NAME) {
        return;
    }
    if std::env::var_os("ZE_ABI_PANIC_CHILD").is_none() {
        let status = Command::new(std::env::current_exe().expect("current test executable"))
            .args(["--exact", NAME, "--nocapture"])
            .env("ZE_ABI_PANIC_CHILD", "1")
            .status()
            .expect("spawn ABI panic child");
        assert!(status.success(), "ABI panic child must survive");
        return;
    }

    let store = common::TestStore::new();
    assert_eq!(trigger_panic(store.handle), ZeErrorCode::ZeErrPanic);
    let mut state: ZeStateReport = common::sized_zeroed();
    assert_eq!(
        ze_state(store.handle, &mut state),
        ZeErrorCode::ZeErrPoisoned
    );

    let mut required = 0;
    assert_eq!(
        ze_last_error_message(store.handle, std::ptr::null_mut(), 0, &mut required),
        ZeErrorCode::ZeOk
    );
    let mut message = vec![0_i8; required + 1];
    assert_eq!(
        ze_last_error_message(
            store.handle,
            message.as_mut_ptr(),
            message.len(),
            &mut required,
        ),
        ZeErrorCode::ZeOk
    );
    let bytes = message
        .iter()
        .take(required)
        .map(|byte| *byte as u8)
        .collect::<Vec<_>>();
    assert_eq!(
        String::from_utf8(bytes).expect("panic UTF-8"),
        "abi panic probe"
    );
    assert_eq!(ze_close(store.handle), ZeErrorCode::ZeErrPoisoned);
}

struct PoisonContext {
    store: common::TestStore,
    vector: Vec<f32>,
    ids: Vec<ZeDocId>,
}

impl PoisonContext {
    fn new() -> Self {
        Self {
            store: common::TestStore::new(),
            vector: vec![1.0],
            ids: vec![ZeDocId { high: 0, low: 1 }],
        }
    }
}

type PoisonCall = fn(&mut PoisonContext) -> ZeErrorCode;

fn poison_function_table() -> Vec<(&'static str, PoisonCall)> {
    vec![
        ("ze_schema_column", |context| {
            let mut report: ZeSchemaColumnResult = common::sized_zeroed();
            ze_schema_column(
                context.store.handle,
                0,
                std::ptr::null_mut(),
                0,
                &mut report,
            )
        }),
        // Root-only export: catches panics but owns no handle to poison.
        ("ze_namespace_declare_cascade", |_| {
            ze_namespace_declare_cascade(std::ptr::null(), std::ptr::null())
        }),
        ("ze_namespace_delete_cascade", |_| {
            ze_namespace_delete_cascade(std::ptr::null())
        }),
        ("ze_namespace_batch", |_| {
            ze_namespace_batch(std::ptr::null())
        }),
        ("ze_state", |context| {
            let mut report: ZeStateReport = common::sized_zeroed();
            ze_state(context.store.handle, &mut report)
        }),
        ("ze_stats", |context| {
            let mut report: ZeStatsReport = common::sized_zeroed();
            ze_stats(context.store.handle, &mut report)
        }),
        ("ze_ingest", |context| {
            let document = ZeIngestDocument {
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
            let request = ZeIngestRequest {
                abi_size: size_of::<ZeIngestRequest>() as u32,
                abi_reserved: 0,
                documents: &document,
                document_count: 1,
                dimension: 1,
            };
            let mut report: ZeMutationReport = common::sized_zeroed();
            ze_ingest(context.store.handle, &request, &mut report)
        }),
        ("ze_upsert", |context| {
            let document = ZeUpsertDocument {
                abi_size: size_of::<ZeUpsertDocument>() as u32,
                abi_reserved: 0,
                document: ZeIngestDocument {
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
                },
                attributes: std::ptr::null(),
                attribute_count: 0,
            };
            let request = ZeUpsertRequest {
                abi_size: size_of::<ZeUpsertRequest>() as u32,
                abi_reserved: 0,
                documents: &document,
                document_count: 1,
                dimension: 1,
            };
            let mut report: ZeMutationReport = common::sized_zeroed();
            ze_upsert(context.store.handle, &request, &mut report)
        }),
        ("ze_upsert_conditional", |context| {
            let document = ZeUpsertDocument {
                abi_size: size_of::<ZeUpsertDocument>() as u32,
                abi_reserved: 0,
                document: ZeIngestDocument {
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
                },
                attributes: std::ptr::null(),
                attribute_count: 0,
            };
            let condition = ZeRevisionCondition {
                kind: ZE_REVISION_CONDITION_NONE,
                reserved: 0,
                revision: 0,
            };
            let request = ZeConditionalUpsertRequest {
                abi_size: size_of::<ZeConditionalUpsertRequest>() as u32,
                abi_reserved: 0,
                batch: ZeUpsertRequest {
                    abi_size: size_of::<ZeUpsertRequest>() as u32,
                    abi_reserved: 0,
                    documents: &document,
                    document_count: 1,
                    dimension: 1,
                },
                conditions: &condition,
                condition_count: 1,
            };
            let mut report: ZeMutationReport = common::sized_zeroed();
            let mut conflict: ZeRevisionConflict = common::sized_zeroed();
            ze_upsert_conditional(context.store.handle, &request, &mut report, &mut conflict)
        }),
        ("ze_get", |context| {
            let request = ZeGetRequest {
                abi_size: size_of::<ZeGetRequest>() as u32,
                abi_reserved: 0,
                ids: context.ids.as_ptr(),
                id_count: context.ids.len(),
                include_vector: 0,
                include_text: 0,
                include_metadata: 0,
                include_attributes: 0,
            };
            let mut result: ZeGetResult = common::sized_zeroed();
            ze_get(context.store.handle, &request, &mut result)
        }),
        ("ze_delete", |context| {
            let request = ZeDeleteRequest {
                abi_size: size_of::<ZeDeleteRequest>() as u32,
                abi_reserved: 0,
                doc_ids: context.ids.as_ptr(),
                doc_id_count: context.ids.len(),
            };
            let mut report: ZeMutationReport = common::sized_zeroed();
            ze_delete(context.store.handle, &request, &mut report)
        }),
        ("ze_delete_where", |context| {
            let request = ZeDeleteWhereRequest {
                abi_size: size_of::<ZeDeleteWhereRequest>() as u32,
                abi_reserved: 0,
                filter: std::ptr::null(),
            };
            let mut report: ZeDeleteWhereReport = common::sized_zeroed();
            ze_delete_where(context.store.handle, &request, &mut report)
        }),
        ("ze_delete_conditional", |context| {
            let condition = ZeRevisionCondition {
                kind: ZE_REVISION_CONDITION_NONE,
                reserved: 0,
                revision: 0,
            };
            let request = ZeConditionalDeleteRequest {
                abi_size: size_of::<ZeConditionalDeleteRequest>() as u32,
                abi_reserved: 0,
                batch: ZeDeleteRequest {
                    abi_size: size_of::<ZeDeleteRequest>() as u32,
                    abi_reserved: 0,
                    doc_ids: context.ids.as_ptr(),
                    doc_id_count: context.ids.len(),
                },
                conditions: &condition,
                condition_count: 1,
            };
            let mut report: ZeMutationReport = common::sized_zeroed();
            let mut conflict: ZeRevisionConflict = common::sized_zeroed();
            ze_delete_conditional(context.store.handle, &request, &mut report, &mut conflict)
        }),
        ("ze_search", |context| {
            let request = common::valid_search_request(&context.vector);
            let mut result: ZeSearchResult = common::sized_zeroed();
            ze_search(context.store.handle, &request, &mut result)
        }),
        ("ze_search_filtered", |context| {
            let nodes = [ZeFilterNode {
                op: 8,
                attribute_id: 0,
                values: std::ptr::null(),
                value_count: 0,
                has_lower: 0,
                lower: unsafe { std::mem::zeroed() },
                lower_inclusive: 0,
                has_upper: 0,
                upper: unsafe { std::mem::zeroed() },
                upper_inclusive: 0,
                children_start: 0,
                children_count: 0,
            }];
            let filter = ZeFilter {
                abi_size: size_of::<ZeFilter>() as u32,
                abi_reserved: 0,
                nodes: nodes.as_ptr(),
                node_count: nodes.len(),
                root: 0,
            };
            let request = ZeSearchFilteredRequest {
                abi_size: size_of::<ZeSearchFilteredRequest>() as u32,
                abi_reserved: 0,
                search: common::valid_search_request(&context.vector),
                filter: &filter,
            };
            let mut result: ZeSearchResult = common::sized_zeroed();
            ze_search_filtered(context.store.handle, &request, &mut result)
        }),
        ("ze_query", |context| {
            let request = common::valid_query_request(&context.vector);
            let mut result: ZeQueryResult = common::sized_zeroed();
            ze_query(context.store.handle, &request, &mut result)
        }),
        ("ze_query_filtered", |context| {
            let request: ZeQueryRequest = common::sized_zeroed();
            let constraints: ZeQueryFilter = common::sized_zeroed();
            let mut result: ZeQueryResult = common::sized_zeroed();
            ze_query_filtered(
                context.store.handle,
                &request,
                &constraints,
                0,
                &mut result,
                std::ptr::null_mut(),
            )
        }),
        ("ze_query_snippet_source_ranges", |_context| {
            let mut ranges: ZeSnippetSourceRanges = common::sized_zeroed();
            ze_query_snippet_source_ranges(std::ptr::null(), 0, &mut ranges)
        }),
        ("ze_query_with_snippets", |context| {
            let request = common::valid_query_request(&context.vector);
            let mut result: ZeQueryResult = common::sized_zeroed();
            let mut snippets: ZeQuerySnippets = common::sized_zeroed();
            ze_query_with_snippets(
                context.store.handle,
                &request,
                64,
                &mut result,
                &mut snippets,
            )
        }),
        ("ze_scan", |context| {
            let request = ZeScanRequest {
                abi_size: size_of::<ZeScanRequest>() as u32,
                abi_reserved: 0,
                cursor_generation: 0,
                cursor_segment_id: [0; 16],
                cursor_next_row: 0,
                cursor_phase: 0,
                limit: 1,
                order: 0,
                include_vector: 0,
                include_text: 0,
                include_metadata: 0,
                include_attributes: 0,
                has_timestamp_range: 0,
                start_ts: 0,
                end_ts: 0,
                filter: std::ptr::null(),
                cancel_token: 0,
                deadline_ns: 0,
            };
            let mut result: ZeScanResult = common::sized_zeroed();
            ze_scan(context.store.handle, &request, &mut result)
        }),
        ("ze_scan_ordered", |context| {
            let request = ZeScanOrderedRequest {
                abi_size: size_of::<ZeScanOrderedRequest>() as u32,
                abi_reserved: 0,
                scan: ZeScanRequest {
                    abi_size: size_of::<ZeScanRequest>() as u32,
                    limit: 1,
                    ..common::sized_zeroed()
                },
                order_attribute_id: 0,
                cursor_order: 0,
                cursor_order_attribute_id: 0,
            };
            let mut result: ZeScanResult = common::sized_zeroed();
            ze_scan_ordered(context.store.handle, &request, &mut result)
        }),
        ("ze_count", |context| {
            let request = ZeCountRequest {
                abi_size: size_of::<ZeCountRequest>() as u32,
                abi_reserved: 0,
                filter: std::ptr::null(),
                has_timestamp_range: 0,
                start_ts: 0,
                end_ts: 0,
            };
            let mut result: ZeCountResult = common::sized_zeroed();
            ze_count(context.store.handle, &request, &mut result)
        }),
        ("ze_count_grouped", |context| {
            let request = ZeCountGroupedRequest {
                abi_size: size_of::<ZeCountGroupedRequest>() as u32,
                abi_reserved: 0,
                count: ZeCountRequest {
                    abi_size: size_of::<ZeCountRequest>() as u32,
                    abi_reserved: 0,
                    filter: std::ptr::null(),
                    has_timestamp_range: 0,
                    start_ts: 0,
                    end_ts: 0,
                },
                group_attribute_id: 0,
                reserved: 0,
                group_limit: 1,
            };
            let mut result: ZeCountGroupedResult = common::sized_zeroed();
            ze_count_grouped(context.store.handle, &request, &mut result)
        }),
        ("ze_scan_result_free", |_context| {
            let mut result: ZeScanResult = common::sized_zeroed();
            ze_scan_result_free(&mut result)
        }),
        ("ze_epoch_current", |context| {
            let mut identity: ZeEpochIdentity = common::sized_zeroed();
            ze_epoch_current(context.store.handle, &mut identity)
        }),
        ("ze_epoch_switch_alias", |context| {
            let epoch = common::EpochFixture::new(1);
            let request = epoch.request();
            let mut report: ZeEpochAliasReport = common::sized_zeroed();
            ze_epoch_switch_alias(context.store.handle, &request, &mut report)
        }),
        ("ze_epoch_drop", |context| {
            let epoch = common::EpochFixture::new(1);
            let request = epoch.request();
            let mut report: ZeEpochDropReport = common::sized_zeroed();
            ze_epoch_drop(context.store.handle, &request, &mut report)
        }),
        ("ze_open_migrations", |context| {
            let mut report: ZeOpenMigrations = common::sized_zeroed();
            ze_open_migrations(context.store.handle, &mut report)
        }),
        ("ze_reindex_text", |context| {
            let mut report: ZeGenerationReport = common::sized_zeroed();
            ze_reindex_text(context.store.handle, &mut report)
        }),
        ("ze_seal", |context| {
            let request = ZeSealRequest {
                abi_size: size_of::<ZeSealRequest>() as u32,
                abi_reserved: 0,
                cancel_token: 0,
            };
            let mut report: ZeGenerationReport = common::sized_zeroed();
            ze_seal(context.store.handle, &request, &mut report)
        }),
        ("ze_merge_sealed", |context| {
            let request = ZeSealRequest {
                abi_size: size_of::<ZeSealRequest>() as u32,
                abi_reserved: 0,
                cancel_token: 0,
            };
            let mut report: ZeGenerationReport = common::sized_zeroed();
            ze_merge_sealed(context.store.handle, &request, &mut report)
        }),
        ("ze_open_snapshot", |context| {
            let mut handle = 0;
            ze_open_snapshot(context.store.handle, &mut handle)
        }),
        ("ze_snapshot", |context| {
            let target = std::env::temp_dir()
                .join("ze-poisoned-snapshot-never-written")
                .to_string_lossy()
                .into_owned();
            let request = ZeSnapshotRequest {
                abi_size: size_of::<ZeSnapshotRequest>() as u32,
                abi_reserved: 0,
                target: target.as_ptr(),
                target_len: target.len(),
            };
            let mut report: ZeGenerationReport = common::sized_zeroed();
            ze_snapshot(context.store.handle, &request, &mut report)
        }),
        ("ze_drop_partition", |context| {
            let request = ZeDropPartitionRequest {
                abi_size: size_of::<ZeDropPartitionRequest>() as u32,
                abi_reserved: 0,
                start_ts: 0,
                end_ts: 1,
            };
            let mut report: ZePartitionReport = common::sized_zeroed();
            ze_drop_partition(context.store.handle, &request, &mut report)
        }),
        ("ze_apply_retention", |context| {
            let request = ZeRetentionRequest {
                abi_size: size_of::<ZeRetentionRequest>() as u32,
                abi_reserved: 0,
                window: 1,
                now_ts: 1,
            };
            let mut report: ZePartitionReport = common::sized_zeroed();
            ze_apply_retention(context.store.handle, &request, &mut report)
        }),
        ("ze_purge", |context| {
            let request = ZePurgeRequest {
                abi_size: size_of::<ZePurgeRequest>() as u32,
                abi_reserved: 0,
                doc_ids: context.ids.as_ptr(),
                doc_id_count: context.ids.len(),
            };
            let mut report: ZePurgeTokenReport = common::sized_zeroed();
            ze_purge(context.store.handle, &request, &mut report)
        }),
        ("ze_await_physical_purge", |context| {
            let request = ZeAwaitPurgeRequest {
                abi_size: size_of::<ZeAwaitPurgeRequest>() as u32,
                abi_reserved: 0,
                token_id: 1,
            };
            let mut report: ZePurgeReport = common::sized_zeroed();
            ze_await_physical_purge(context.store.handle, &request, &mut report)
        }),
        ("ze_maintain", |context| {
            let request = ZeMaintainRequest {
                abi_size: size_of::<ZeMaintainRequest>() as u32,
                abi_reserved: 0,
                wall_time_ns: 1,
                bytes: 1,
            };
            let mut report: ZeMaintainReport = common::sized_zeroed();
            ze_maintain(context.store.handle, &request, &mut report)
        }),
        ("ze_close", |context| ze_close(context.store.handle)),
    ]
}

#[test]
fn every_entry_point_returns_ze_err_poisoned_after_a_caught_panic() {
    let _process_guard = process_sensitive_lock();
    const NAME: &str = "every_entry_point_returns_ze_err_poisoned_after_a_caught_panic";
    if delegate_to_panic_feature(NAME) {
        return;
    }
    let table = poison_function_table();
    assert_eq!(table.len(), POISON_TABLE_NAMES.len());
    for (name, call) in table {
        let mut context = PoisonContext::new();
        arm_named_panic(name);
        assert_eq!(
            call(&mut context),
            ZeErrorCode::ZeErrPanic,
            "{name} catch wrapper"
        );
        if matches!(
            name,
            "ze_scan_result_free"
                | "ze_query_snippet_source_ranges"
                | "ze_namespace_batch"
                | "ze_namespace_declare_cascade"
                | "ze_namespace_delete_cascade"
        ) {
            assert_eq!(
                call(&mut context),
                ZeErrorCode::ZeErrInvalidArgument,
                "{name}"
            );
            continue;
        }
        assert_eq!(call(&mut context), ZeErrorCode::ZeErrPoisoned, "{name}");
    }
}

#[test]
fn the_poison_table_covers_every_exported_handle_taking_symbol() {
    let header = std::fs::read_to_string(
        std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("include/zeppelin_embed.h"),
    )
    .expect("committed C header");
    let mut declarations = String::new();
    let mut remainder = header.as_str();
    while let Some(start) = remainder.find("/*") {
        declarations.push_str(&remainder[..start]);
        let after_start = &remainder[start + 2..];
        let end = after_start.find("*/").expect("terminated header comment");
        remainder = &after_start[end + 2..];
    }
    declarations.push_str(remainder);
    let mut exported = std::collections::BTreeSet::new();
    for declaration in declarations.split(';') {
        if !declaration.contains("ze_handle handle") {
            continue;
        }
        let Some(open) = declaration.find('(') else {
            continue;
        };
        let name = declaration[..open]
            .split_whitespace()
            .last()
            .expect("function name")
            .to_owned();
        if name != "ze_last_error_message" && !name.starts_with("ze_text_") {
            exported.insert(name);
        }
    }
    let complete_table = poison_function_table()
        .into_iter()
        .map(|(name, _)| name.to_owned())
        .collect::<std::collections::BTreeSet<_>>();
    assert_eq!(
        complete_table,
        POISON_TABLE_NAMES
            .iter()
            .map(|name| (*name).to_owned())
            .collect()
    );
    let handle_table = complete_table
        .into_iter()
        .filter(|name| {
            !matches!(
                name.as_str(),
                "ze_scan_result_free"
                    | "ze_query_snippet_source_ranges"
                    | "ze_namespace_batch"
                    | "ze_namespace_declare_cascade"
                    | "ze_namespace_delete_cascade"
            )
        })
        .collect::<std::collections::BTreeSet<_>>();
    assert_eq!(exported, handle_table);
}

#[test]
fn a_second_concurrent_writer_call_returns_ze_err_busy() {
    let store = common::TestStore::new();
    let handle = store.handle;
    let (started_tx, started_rx) = std::sync::mpsc::channel();
    let writer = std::thread::spawn(move || {
        started_tx.send(()).expect("signal writer start");
        common::ingest_rows(handle, 500, 256)
    });
    started_rx.recv().expect("writer start");
    std::thread::sleep(std::time::Duration::from_millis(1));

    let request = ZeMaintainRequest {
        abi_size: size_of::<ZeMaintainRequest>() as u32,
        abi_reserved: 0,
        wall_time_ns: 1,
        bytes: 1,
    };
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
    let observed = loop {
        let mut report: ZeMaintainReport = common::sized_zeroed();
        let code = ze_maintain(handle, &request, &mut report);
        if code == ZeErrorCode::ZeErrBusy {
            break code;
        }
        assert_eq!(
            code,
            ZeErrorCode::ZeOk,
            "second writer returned a typed result"
        );
        assert!(
            std::time::Instant::now() < deadline,
            "second writer never observed ZE_ERR_BUSY before the hard deadline"
        );
        std::thread::yield_now();
    };
    assert_eq!(observed, ZeErrorCode::ZeErrBusy);
    assert_eq!(writer.join().expect("writer thread"), ZeErrorCode::ZeOk);
}

#[test]
fn a_second_process_opening_the_same_path_gets_store_busy() {
    let _process_guard = process_sensitive_lock();
    const NAME: &str = "a_second_process_opening_the_same_path_gets_store_busy";
    if let Some(path) = std::env::var_os("ZE_STORE_BUSY_CHILD_PATH") {
        let (code, handle) = common::open_path(std::path::Path::new(&path));
        assert_eq!(code, ZeErrorCode::ZeErrStoreBusy);
        assert_eq!(handle, 0);
        return;
    }
    let store = common::TestStore::new();
    let status = Command::new(std::env::current_exe().expect("current test executable"))
        .args(["--exact", NAME, "--nocapture"])
        .env("ZE_STORE_BUSY_CHILD_PATH", &store.path)
        .status()
        .expect("spawn second-process open");
    assert!(status.success(), "second-process open child failed");
}

#[test]
fn cross_thread_cancellation_returns_typed_cancelled_with_no_candidates_through_the_boundary() {
    let store = common::TestStore::new();
    assert_eq!(
        common::ingest_rows(store.handle, 100, 8_192),
        ZeErrorCode::ZeOk
    );
    let mut token = 0;
    assert_eq!(ze_cancel_token_create(&mut token), ZeErrorCode::ZeOk);
    let canceller = std::thread::spawn(move || ze_cancel_token_cancel(token));
    assert_eq!(
        canceller.join().expect("cancellation thread"),
        ZeErrorCode::ZeOk
    );

    let vector = vec![0.25_f32; 8_192];
    let mut request = common::valid_search_request(&vector);
    request.k = 100;
    request.thread_budget = 1;
    request.cancel_token = token;
    let mut result: ZeSearchResult = common::sized_zeroed();
    assert_eq!(
        ze_search(store.handle, &request, &mut result),
        ZeErrorCode::ZeErrCancelled
    );
    assert_eq!(result.hit_count, 0);
    assert!(result.hits.is_null());
    assert_eq!(ze_cancel_token_free(token), ZeErrorCode::ZeOk);
}

#[test]
fn a_deadline_mid_query_returns_typed_timeout_through_the_boundary() {
    let store = common::TestStore::new();
    assert_eq!(
        common::ingest_rows(store.handle, 100, 8_192),
        ZeErrorCode::ZeOk
    );
    let vector = vec![0.25_f32; 8_192];
    let mut request = common::valid_search_request(&vector);
    request.k = 100;
    request.deadline_ns = 1;
    let mut result: ZeSearchResult = common::sized_zeroed();
    assert_eq!(
        ze_search(store.handle, &request, &mut result),
        ZeErrorCode::ZeErrTimeout
    );
    assert_eq!(result.hit_count, 0);
    assert!(result.hits.is_null());
}

#[test]
fn search_result_ownership_and_the_phase_one_engine_seams_are_real() {
    let store = common::TestStore::new();
    let mut state: ZeStateReport = common::sized_zeroed();
    assert_eq!(ze_state(store.handle, &mut state), ZeErrorCode::ZeOk);
    assert_eq!(state.state, 0);
    let mut stats: ZeStatsReport = common::sized_zeroed();
    assert_eq!(ze_stats(store.handle, &mut stats), ZeErrorCode::ZeOk);

    assert_eq!(common::ingest_rows(store.handle, 2, 4), ZeErrorCode::ZeOk);
    let deleted = [ZeDocId { high: 0, low: 1 }];
    let delete = ZeDeleteRequest {
        abi_size: size_of::<ZeDeleteRequest>() as u32,
        abi_reserved: 0,
        doc_ids: deleted.as_ptr(),
        doc_id_count: deleted.len(),
    };
    let mut mutation: ZeMutationReport = common::sized_zeroed();
    assert_eq!(
        ze_delete(store.handle, &delete, &mut mutation),
        ZeErrorCode::ZeOk
    );
    let vector = vec![0.25_f32; 4];
    let request = common::valid_search_request(&vector);
    let mut result: ZeSearchResult = common::sized_zeroed();
    assert_eq!(
        ze_search(store.handle, &request, &mut result),
        ZeErrorCode::ZeOk
    );
    assert_eq!(result.hit_count, 1);
    assert!(!result.hits.is_null());
    assert_eq!(ze_search_result_free(&mut result), ZeErrorCode::ZeOk);
    assert_eq!(ze_search_result_free(&mut result), ZeErrorCode::ZeOk);
    let mut zeroed: ZeSearchResult = unsafe { std::mem::zeroed() };
    assert_eq!(ze_search_result_free(&mut zeroed), ZeErrorCode::ZeOk);

    let seal = ZeSealRequest {
        abi_size: size_of::<ZeSealRequest>() as u32,
        abi_reserved: 0,
        cancel_token: 0,
    };
    let mut generation: ZeGenerationReport = common::sized_zeroed();
    assert_eq!(
        ze_seal(store.handle, &seal, &mut generation),
        ZeErrorCode::ZeOk
    );

    let drop_request = ZeDropPartitionRequest {
        abi_size: size_of::<ZeDropPartitionRequest>() as u32,
        abi_reserved: 0,
        start_ts: i64::MIN,
        end_ts: 1,
    };
    let mut partition: ZePartitionReport = common::sized_zeroed();
    assert_eq!(
        ze_drop_partition(store.handle, &drop_request, &mut partition),
        ZeErrorCode::ZeOk
    );

    let retention = ZeRetentionRequest {
        abi_size: size_of::<ZeRetentionRequest>() as u32,
        abi_reserved: 0,
        window: 10,
        now_ts: 10,
    };
    assert_eq!(
        ze_apply_retention(store.handle, &retention, &mut partition),
        ZeErrorCode::ZeOk
    );

    let ids = [ZeDocId {
        high: u64::MAX,
        low: u64::MAX,
    }];
    let purge = ZePurgeRequest {
        abi_size: size_of::<ZePurgeRequest>() as u32,
        abi_reserved: 0,
        doc_ids: ids.as_ptr(),
        doc_id_count: ids.len(),
    };
    let mut token: ZePurgeTokenReport = common::sized_zeroed();
    assert_eq!(
        ze_purge(store.handle, &purge, &mut token),
        ZeErrorCode::ZeOk
    );
    assert_eq!(token.is_no_op, 1);
    let await_request = ZeAwaitPurgeRequest {
        abi_size: size_of::<ZeAwaitPurgeRequest>() as u32,
        abi_reserved: 0,
        token_id: token.token_id,
    };
    let mut purge_report: ZePurgeReport = common::sized_zeroed();
    assert_eq!(
        ze_await_physical_purge(store.handle, &await_request, &mut purge_report),
        ZeErrorCode::ZeOk
    );

    let maintain = ZeMaintainRequest {
        abi_size: size_of::<ZeMaintainRequest>() as u32,
        abi_reserved: 0,
        wall_time_ns: 0,
        bytes: 0,
    };
    let mut maintenance: ZeMaintainReport = common::sized_zeroed();
    assert_eq!(
        ze_maintain(store.handle, &maintain, &mut maintenance),
        ZeErrorCode::ZeOk
    );
    assert_eq!(maintenance.status, 1);
}

#[test]
fn namespace_tokenizer_open_catches_its_named_panic_probe() {
    let _process_guard = process_sensitive_lock();
    const NAME: &str = "namespace_tokenizer_open_catches_its_named_panic_probe";
    if delegate_to_panic_feature(NAME) {
        return;
    }
    arm_named_panic("ze_namespace_open_with_tokenizer");
    assert_eq!(
        ze_namespace_open_with_tokenizer(std::ptr::null(), 2, std::ptr::null_mut()),
        ZeErrorCode::ZeErrPanic
    );
    assert_eq!(
        ze_namespace_open_with_tokenizer(std::ptr::null(), 2, std::ptr::null_mut()),
        ZeErrorCode::ZeErrInvalidArgument
    );
}

#[test]
fn open_migrations_layout_is_frozen() {
    assert_eq!(size_of::<ZeOpenMigrations>(), 24);
    assert_eq!(offset_of!(ZeOpenMigrations, abi_size), 0);
    assert_eq!(offset_of!(ZeOpenMigrations, abi_reserved), 4);
    assert_eq!(offset_of!(ZeOpenMigrations, generation), 8);
    assert_eq!(offset_of!(ZeOpenMigrations, changes), 16);
    assert_eq!(offset_of!(ZeOpenMigrations, manifest_version), 20);
    assert_eq!(offset_of!(ZeOpenMigrations, wal_version), 22);
}

// Creation takes no existing handle, so this export is deliberately outside
// poison_function_table: there is no existing owner for it to poison.
#[cfg(feature = "graph-cypher")]
const GRAPH_HANDLE_FREE_EXPORTS: &[&str] = &["ze_graph_open_with_relationship_types"];

#[cfg(feature = "graph-cypher")]
#[test]
fn graph_declaration_export_is_registered_without_a_poison_handle() {
    let header = include_str!("../include/zeppelin_graph_contracts.h");
    let allowlist = include_str!("../symbols.allowlist");
    for name in GRAPH_HANDLE_FREE_EXPORTS {
        assert!(header.contains(&format!("ze_error_code {name}(")));
        assert!(allowlist.lines().any(|symbol| symbol == *name));
        assert!(!POISON_TABLE_NAMES.contains(name));
    }
    assert_eq!(
        ze_graph_open_with_relationship_types(
            std::ptr::null(),
            std::ptr::null(),
            0,
            std::ptr::null_mut()
        ),
        ZeErrorCode::ZeErrInvalidArgument
    );
}

#[test]
fn namespace_batch_structs_have_frozen_layouts() {
    assert_layout!(ZeNamespaceMutation, 120, 8, {
        abi_size: 0, abi_reserved: 4, name: 8, name_len: 16, spec: 24,
        tokenizer_profile: 32, reserved: 36, upserts: 40, deletes: 96,
        delete_count: 104, filter: 112
    });
    assert_layout!(ZeNamespaceBatchRequest, 48, 8, {
        abi_size: 0, abi_reserved: 4, root: 8, root_len: 16,
        participants: 24, participant_count: 32, generations: 40
    });
}

#[test]
fn cascade_declaration_has_frozen_layout() {
    assert_layout!(ZeCascadeDeclaration, 20, 4, {
        abi_size: 0, abi_reserved: 4, parent_index: 8, child_index: 12, attribute_id: 16
    });
}

// Graph handles have their own registry; never send them through legacy poison calls.
#[cfg(feature = "graph-cypher")]
const GRAPH_MAINTENANCE_POISON_TABLE: &[&str] =
    &["ze_graph_maintain", "ze_graph_set_maintenance_policy"];

#[cfg(feature = "graph-cypher")]
#[test]
fn graph_maintenance_exports_and_frozen_layouts() {
    assert_layout!(ZeGraphMaintenancePolicy, 16, 8, { abi_size: 0, automatic: 4, reclaim_after_bytes: 8 });
    assert_layout!(ZeGraphMaintainReport, 64, 8, { abi_size: 0, cycle_complete: 4, generation: 8, replaced_physical_refs: 16, new_pack_bytes: 24, relocated_bytes: 32, drained_packs: 40, reclaimed_bytes: 48, removed_bytes: 56 });
    for name in GRAPH_MAINTENANCE_POISON_TABLE {
        assert!(
            include_str!("../include/zeppelin_graph_contracts.h")
                .contains(&format!("ze_error_code {name}("))
        );
        assert!(
            include_str!("../symbols.allowlist")
                .lines()
                .any(|symbol| symbol == *name)
        );
        assert!(include_str!("ffi_graph_poison.rs").contains(&format!("\"{name}\"")));
    }
}
