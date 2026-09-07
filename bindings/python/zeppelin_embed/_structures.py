"""Exact ``ctypes`` mirrors of the frozen Zeppelin Embed C ABI."""

from __future__ import annotations

import ctypes as ct
from typing import TypeVar

UInt8Pointer = ct.POINTER(ct.c_uint8)
FloatPointer = ct.POINTER(ct.c_float)


class ZeOpenRequest(ct.Structure):
    _fields_ = [
        ("abi_size", ct.c_uint32),
        ("abi_reserved", ct.c_uint32),
        ("path", UInt8Pointer),
        ("path_len", ct.c_size_t),
        ("access_mode", ct.c_int32),
        ("durability_mode", ct.c_int32),
        ("commit_tier", ct.c_int32),
        ("reader_drain_timeout_ms", ct.c_uint64),
        ("max_resident_bytes", ct.c_uint64),
        ("max_temp_bytes", ct.c_uint64),
    ]


class ZeEmbeddingTower(ct.Structure):
    _fields_ = [
        ("model_id", UInt8Pointer),
        ("model_id_len", ct.c_size_t),
        ("model_version", UInt8Pointer),
        ("model_version_len", ct.c_size_t),
        ("weights_digest", UInt8Pointer),
        ("weights_digest_len", ct.c_size_t),
        ("dims", ct.c_uint32),
        ("normalization", ct.c_int32),
        ("prompt_prefix", UInt8Pointer),
        ("prompt_prefix_len", ct.c_size_t),
        ("max_tokens", ct.c_uint32),
        ("runtime", ct.c_int32),
        ("compute_units", ct.c_int32),
        ("has_os_build", ct.c_uint32),
        ("os_build", UInt8Pointer),
        ("os_build_len", ct.c_size_t),
    ]


class ZeEmbeddingEpoch(ct.Structure):
    _fields_ = [
        ("document", ZeEmbeddingTower),
        ("query", ZeEmbeddingTower),
        ("alignment_digest", UInt8Pointer),
        ("alignment_digest_len", ct.c_size_t),
    ]


class ZeEpochRequest(ct.Structure):
    _fields_ = [
        ("abi_size", ct.c_uint32),
        ("abi_reserved", ct.c_uint32),
        ("embedding", ZeEmbeddingEpoch),
        ("tokenizer_profile", ct.c_int32),
        ("reserved", ct.c_uint32),
    ]


class ZeAttributeDefinition(ct.Structure):
    _fields_ = [
        ("attribute_id", ct.c_uint32),
        ("name", UInt8Pointer),
        ("name_len", ct.c_size_t),
        ("attribute_type", ct.c_int32),
        ("nullable", ct.c_uint32),
    ]


class ZeNamespaceSpec(ct.Structure):
    _fields_ = [
        ("abi_size", ct.c_uint32),
        ("abi_reserved", ct.c_uint32),
        ("attributes", ct.POINTER(ZeAttributeDefinition)),
        ("attribute_count", ct.c_size_t),
        ("has_vector_space", ct.c_uint32),
        ("dimensions", ct.c_uint32),
        ("normalization", ct.c_int32),
        ("epoch", ct.POINTER(ZeEpochRequest)),
    ]


class ZeNamespaceOpenRequest(ct.Structure):
    _fields_ = [
        ("abi_size", ct.c_uint32),
        ("abi_reserved", ct.c_uint32),
        ("root", UInt8Pointer),
        ("root_len", ct.c_size_t),
        ("name", UInt8Pointer),
        ("name_len", ct.c_size_t),
        ("open", ZeOpenRequest),
        ("spec", ct.POINTER(ZeNamespaceSpec)),
    ]


class ZeNamespaceListRequest(ct.Structure):
    _fields_ = [
        ("abi_size", ct.c_uint32),
        ("abi_reserved", ct.c_uint32),
        ("root", UInt8Pointer),
        ("root_len", ct.c_size_t),
    ]


class ZeNamespaceEntry(ct.Structure):
    _fields_ = [("name", UInt8Pointer), ("name_len", ct.c_size_t)]


class ZeNamespaceListResult(ct.Structure):
    _fields_ = [
        ("abi_size", ct.c_uint32),
        ("abi_reserved", ct.c_uint32),
        ("entries", ct.POINTER(ZeNamespaceEntry)),
        ("entry_count", ct.c_size_t),
    ]


class ZeEpochIdentity(ct.Structure):
    _fields_ = [
        ("abi_size", ct.c_uint32),
        ("abi_reserved", ct.c_uint32),
        ("embedding_epoch", ct.c_uint64),
        ("tokenizer_epoch", ct.c_uint64),
    ]


class ZeEpochAliasReport(ct.Structure):
    _fields_ = [
        ("abi_size", ct.c_uint32),
        ("abi_reserved", ct.c_uint32),
        ("generation", ct.c_uint64),
        ("previous_embedding_epoch", ct.c_uint64),
        ("previous_tokenizer_epoch", ct.c_uint64),
        ("published_embedding_epoch", ct.c_uint64),
        ("published_tokenizer_epoch", ct.c_uint64),
        ("manifest_committed", ct.c_uint32),
        ("reserved", ct.c_uint32),
    ]


class ZeEpochDropReport(ct.Structure):
    _fields_ = [
        ("abi_size", ct.c_uint32),
        ("abi_reserved", ct.c_uint32),
        ("generation", ct.c_uint64),
        ("segments_dropped", ct.c_uint64),
        ("bytes_reclaimed", ct.c_uint64),
    ]


class ZeStateReport(ct.Structure):
    _fields_ = [
        ("abi_size", ct.c_uint32),
        ("abi_reserved", ct.c_uint32),
        ("state", ct.c_int32),
        ("reserved", ct.c_uint32),
    ]


class ZeStatsReport(ct.Structure):
    _fields_ = [
        ("abi_size", ct.c_uint32),
        ("abi_reserved", ct.c_uint32),
        ("resident_owned_bytes", ct.c_uint64),
        ("mapped_bytes", ct.c_uint64),
        ("mapped_resident_bytes", ct.c_uint64),
        ("segment_bytes", ct.c_uint64),
        ("active_segment_bytes", ct.c_uint64),
        ("active_row_count", ct.c_uint64),
        ("tombstone_count", ct.c_uint64),
        ("tombstone_bytes", ct.c_uint64),
        ("wal_bytes", ct.c_uint64),
        ("cache_bytes", ct.c_uint64),
        ("temporary_bytes", ct.c_uint64),
        ("query_pool_bytes", ct.c_uint64),
        ("open_files", ct.c_uint64),
        ("active_queries", ct.c_uint64),
        ("active_snapshot_leases", ct.c_uint64),
        ("phys_footprint", ct.c_uint64),
        ("has_phys_footprint", ct.c_uint32),
        ("reserved", ct.c_uint32),
    ]


class ZeDocId(ct.Structure):
    _fields_ = [("high", ct.c_uint64), ("low", ct.c_uint64)]


class ZeIngestDocument(ct.Structure):
    _fields_ = [
        ("abi_size", ct.c_uint32),
        ("abi_reserved", ct.c_uint32),
        ("doc_id", ZeDocId),
        ("revision", ct.c_uint64),
        ("timestamp", ct.c_int64),
        ("vector", FloatPointer),
        ("vector_len", ct.c_size_t),
        ("metadata", UInt8Pointer),
        ("metadata_len", ct.c_size_t),
        ("text", UInt8Pointer),
        ("text_len", ct.c_size_t),
    ]


class ZeTextOpenRequest(ct.Structure):
    _fields_ = [
        ("abi_size", ct.c_uint32),
        ("abi_reserved", ct.c_uint32),
        ("store", ZeOpenRequest),
        ("bundle_path", UInt8Pointer),
        ("bundle_path_len", ct.c_size_t),
    ]


class ZeTextDocument(ct.Structure):
    _fields_ = [
        ("abi_size", ct.c_uint32),
        ("abi_reserved", ct.c_uint32),
        ("doc_id", ZeDocId),
        ("revision", ct.c_uint64),
        ("text", UInt8Pointer),
        ("text_len", ct.c_size_t),
    ]


class ZeTextIngestRequest(ct.Structure):
    _fields_ = [
        ("abi_size", ct.c_uint32),
        ("abi_reserved", ct.c_uint32),
        ("documents", ct.POINTER(ZeTextDocument)),
        ("document_count", ct.c_size_t),
        ("embed_batch_size", ct.c_size_t),
        ("seal_every", ct.c_size_t),
        ("channel_capacity", ct.c_size_t),
    ]


class ZeTextQueryRequest(ct.Structure):
    _fields_ = [
        ("abi_size", ct.c_uint32),
        ("abi_reserved", ct.c_uint32),
        ("text", UInt8Pointer),
        ("text_len", ct.c_size_t),
        ("k", ct.c_size_t),
        ("legs", ct.c_int32),
        ("reserved", ct.c_uint32),
    ]


class ZeTextQueryHit(ct.Structure):
    _fields_ = [
        ("doc_id", ZeDocId),
        ("revision", ct.c_uint64),
        ("chunk", ct.c_uint32),
        ("reserved", ct.c_uint32),
        ("text", UInt8Pointer),
        ("text_len", ct.c_size_t),
        ("score", ct.c_double),
        ("has_vector_score", ct.c_uint32),
        ("has_lexical_score", ct.c_uint32),
        ("vector_squared_l2", ct.c_double),
        ("lexical_bm25", ct.c_double),
    ]


class ZeTextQueryResult(ct.Structure):
    _fields_ = [
        ("abi_size", ct.c_uint32),
        ("abi_reserved", ct.c_uint32),
        ("hits", ct.POINTER(ZeTextQueryHit)),
        ("hit_count", ct.c_size_t),
        ("embedding_epoch", ct.c_uint64),
        ("tokenizer_epoch", ct.c_uint64),
    ]


class ZeIngestRequest(ct.Structure):
    pass


ZeIngestRequest._fields_ = [
    ("abi_size", ct.c_uint32),
    ("abi_reserved", ct.c_uint32),
    ("documents", ct.POINTER(ZeIngestDocument)),
    ("document_count", ct.c_size_t),
    ("dimension", ct.c_size_t),
]


class ZeAttributeValue(ct.Structure):
    _fields_ = [
        ("attribute_id", ct.c_uint32),
        ("value_type", ct.c_int32),
        ("u64_value", ct.c_uint64),
        ("i64_value", ct.c_int64),
        ("f64_value", ct.c_double),
        ("bool_value", ct.c_uint32),
        ("string_value", UInt8Pointer),
        ("string_len", ct.c_size_t),
    ]


class ZeUpsertDocument(ct.Structure):
    _fields_ = [
        ("abi_size", ct.c_uint32),
        ("abi_reserved", ct.c_uint32),
        ("document", ZeIngestDocument),
        ("attributes", ct.POINTER(ZeAttributeValue)),
        ("attribute_count", ct.c_size_t),
    ]


class ZeUpsertRequest(ct.Structure):
    _fields_ = [
        ("abi_size", ct.c_uint32),
        ("abi_reserved", ct.c_uint32),
        ("documents", ct.POINTER(ZeUpsertDocument)),
        ("document_count", ct.c_size_t),
        ("dimension", ct.c_size_t),
    ]


class ZeGetRequest(ct.Structure):
    _fields_ = [
        ("abi_size", ct.c_uint32),
        ("abi_reserved", ct.c_uint32),
        ("ids", ct.POINTER(ZeDocId)),
        ("id_count", ct.c_size_t),
        ("include_vector", ct.c_uint32),
        ("include_text", ct.c_uint32),
        ("include_metadata", ct.c_uint32),
        ("include_attributes", ct.c_uint32),
    ]


class ZeStoredDocument(ct.Structure):
    _fields_ = [
        ("has_document", ct.c_uint32),
        ("doc_id", ZeDocId),
        ("revision", ct.c_uint64),
        ("timestamp", ct.c_int64),
        ("vector", FloatPointer),
        ("vector_len", ct.c_size_t),
        ("text", UInt8Pointer),
        ("text_len", ct.c_size_t),
        ("metadata", UInt8Pointer),
        ("metadata_len", ct.c_size_t),
        ("attributes", ct.POINTER(ZeAttributeValue)),
        ("attribute_count", ct.c_size_t),
    ]


class ZeGetResult(ct.Structure):
    _fields_ = [
        ("abi_size", ct.c_uint32),
        ("abi_reserved", ct.c_uint32),
        ("documents", ct.POINTER(ZeStoredDocument)),
        ("document_count", ct.c_size_t),
        ("missing_count", ct.c_size_t),
        ("generation", ct.c_uint64),
    ]


class ZeFilterNode(ct.Structure):
    _fields_ = [
        ("op", ct.c_int32),
        ("attribute_id", ct.c_uint32),
        ("values", ct.POINTER(ZeAttributeValue)),
        ("value_count", ct.c_size_t),
        ("has_lower", ct.c_uint32),
        ("lower", ZeAttributeValue),
        ("lower_inclusive", ct.c_uint32),
        ("has_upper", ct.c_uint32),
        ("upper", ZeAttributeValue),
        ("upper_inclusive", ct.c_uint32),
        ("children_start", ct.c_uint32),
        ("children_count", ct.c_uint32),
    ]


class ZeFilter(ct.Structure):
    _fields_ = [
        ("abi_size", ct.c_uint32),
        ("abi_reserved", ct.c_uint32),
        ("nodes", ct.POINTER(ZeFilterNode)),
        ("node_count", ct.c_size_t),
        ("root", ct.c_uint32),
    ]


class ZeScanRequest(ct.Structure):
    _fields_ = [
        ("abi_size", ct.c_uint32),
        ("abi_reserved", ct.c_uint32),
        ("cursor_generation", ct.c_uint64),
        ("cursor_segment_id", ct.c_uint8 * 16),
        ("cursor_next_row", ct.c_uint32),
        ("cursor_phase", ct.c_uint32),
        ("limit", ct.c_size_t),
        ("order", ct.c_int32),
        ("include_vector", ct.c_uint32),
        ("include_text", ct.c_uint32),
        ("include_metadata", ct.c_uint32),
        ("include_attributes", ct.c_uint32),
        ("has_timestamp_range", ct.c_uint32),
        ("start_ts", ct.c_int64),
        ("end_ts", ct.c_int64),
        ("filter", ct.POINTER(ZeFilter)),
        ("cancel_token", ct.c_uint64),
        ("deadline_ns", ct.c_uint64),
    ]


class ZeScanResult(ct.Structure):
    _fields_ = [
        ("abi_size", ct.c_uint32),
        ("abi_reserved", ct.c_uint32),
        ("documents", ct.POINTER(ZeStoredDocument)),
        ("document_count", ct.c_size_t),
        ("generation", ct.c_uint64),
        ("has_more", ct.c_uint32),
        ("next_segment_id", ct.c_uint8 * 16),
        ("next_row", ct.c_uint32),
        ("next_phase", ct.c_uint32),
    ]


class ZeCountRequest(ct.Structure):
    _fields_ = [
        ("abi_size", ct.c_uint32),
        ("abi_reserved", ct.c_uint32),
        ("filter", ct.POINTER(ZeFilter)),
        ("has_timestamp_range", ct.c_uint32),
        ("start_ts", ct.c_int64),
        ("end_ts", ct.c_int64),
    ]


class ZeCountResult(ct.Structure):
    _fields_ = [
        ("abi_size", ct.c_uint32),
        ("abi_reserved", ct.c_uint32),
        ("count", ct.c_uint64),
        ("generation", ct.c_uint64),
    ]


class ZeMutationReport(ct.Structure):
    _fields_ = [
        ("abi_size", ct.c_uint32),
        ("abi_reserved", ct.c_uint32),
        ("sequence", ct.c_uint64),
        ("generation", ct.c_uint64),
    ]


class ZeDeleteRequest(ct.Structure):
    _fields_ = [
        ("abi_size", ct.c_uint32),
        ("abi_reserved", ct.c_uint32),
        ("doc_ids", ct.POINTER(ZeDocId)),
        ("doc_id_count", ct.c_size_t),
    ]


class ZeSearchRequest(ct.Structure):
    _fields_ = [
        ("abi_size", ct.c_uint32),
        ("abi_reserved", ct.c_uint32),
        ("vector", FloatPointer),
        ("vector_len", ct.c_size_t),
        ("dimension", ct.c_size_t),
        ("k", ct.c_size_t),
        ("thread_budget", ct.c_size_t),
        ("has_tier", ct.c_uint32),
        ("tier", ct.c_int32),
        ("graph_profile", ct.c_int32),
        ("reserved", ct.c_uint32),
        ("graph_ef", ct.c_size_t),
        ("graph_seed", ct.c_uint64),
        ("cancel_token", ct.c_uint64),
        ("deadline_ns", ct.c_uint64),
    ]


class ZeSearchFilteredRequest(ct.Structure):
    _fields_ = [
        ("abi_size", ct.c_uint32),
        ("abi_reserved", ct.c_uint32),
        ("search", ZeSearchRequest),
        ("filter", ct.POINTER(ZeFilter)),
    ]


class ZeSearchHit(ct.Structure):
    _fields_ = [
        ("source_kind", ct.c_uint32),
        ("reserved", ct.c_uint32),
        ("segment_id", ct.c_uint8 * 16),
        ("local_row", ct.c_uint32),
        ("has_document", ct.c_uint32),
        ("doc_id", ZeDocId),
        ("revision", ct.c_uint64),
        ("score", ct.c_float),
        ("reserved_tail", ct.c_uint32),
    ]


class ZeSearchResult(ct.Structure):
    pass


ZeSearchResult._fields_ = [
    ("abi_size", ct.c_uint32),
    ("abi_reserved", ct.c_uint32),
    ("hits", ct.POINTER(ZeSearchHit)),
    ("hit_count", ct.c_size_t),
    ("generation", ct.c_uint64),
    ("dims_touched", ct.c_uint64),
    ("bytes_read", ct.c_uint64),
    ("threads_used", ct.c_uint64),
    ("graph_segments_traversed", ct.c_uint64),
    ("graph_validations", ct.c_uint64),
    ("graph_entry_seed_discoveries", ct.c_uint64),
    ("graph_visited_epoch_clears", ct.c_uint64),
    ("graph_candidates_scored", ct.c_uint64),
    ("graph_candidates_rescored", ct.c_uint64),
    ("graph_segments_pruned_by_bound", ct.c_uint64),
]


class ZeQueryRequest(ct.Structure):
    _fields_ = [
        ("abi_size", ct.c_uint32),
        ("abi_reserved", ct.c_uint32),
        ("vector", FloatPointer),
        ("vector_len", ct.c_size_t),
        ("dimension", ct.c_size_t),
        ("text", UInt8Pointer),
        ("text_len", ct.c_size_t),
        ("k", ct.c_size_t),
        ("thread_budget", ct.c_size_t),
        ("has_tier", ct.c_uint32),
        ("tier", ct.c_int32),
        ("graph_profile", ct.c_int32),
        ("lexical_flags", ct.c_uint32),
        ("graph_ef", ct.c_size_t),
        ("graph_seed", ct.c_uint64),
        ("has_alpha", ct.c_uint32),
        ("rules_enabled", ct.c_uint32),
        ("alpha", ct.c_double),
        ("has_max_rounds", ct.c_uint32),
        ("quoted_phrase", ct.c_uint32),
        ("max_rounds", ct.c_uint64),
        ("identifier_token", ct.c_uint32),
        ("has_rarest_exact_document_frequency", ct.c_uint32),
        ("rarest_exact_document_frequency", ct.c_uint64),
        ("cancel_token", ct.c_uint64),
        ("deadline_ns", ct.c_uint64),
    ]


class ZeQueryHit(ct.Structure):
    _fields_ = [
        ("has_document", ct.c_uint32),
        ("has_revision", ct.c_uint32),
        ("doc_id", ZeDocId),
        ("revision", ct.c_uint64),
        ("score", ct.c_double),
        ("has_vector_score", ct.c_uint32),
        ("has_lexical_score", ct.c_uint32),
        ("vector_squared_l2", ct.c_double),
        ("lexical_bm25", ct.c_double),
    ]


class ZeQueryResult(ct.Structure):
    pass


ZeQueryResult._fields_ = [
    ("abi_size", ct.c_uint32),
    ("abi_reserved", ct.c_uint32),
    ("hits", ct.POINTER(ZeQueryHit)),
    ("hit_count", ct.c_size_t),
    ("generation", ct.c_uint64),
    ("mode", ct.c_int32),
    ("approximate", ct.c_uint32),
    ("exact_rescore", ct.c_uint32),
    ("budget_exhausted", ct.c_uint32),
    ("has_fusion", ct.c_uint32),
    ("fusion_method", ct.c_int32),
    ("effective_alpha", ct.c_double),
    ("fusion_rounds", ct.c_uint64),
    ("has_embedding_epoch", ct.c_uint32),
    ("has_tokenizer_epoch", ct.c_uint32),
    ("embedding_epoch", ct.c_uint64),
    ("tokenizer_epoch", ct.c_uint64),
    ("dims_touched", ct.c_uint64),
    ("bytes_read", ct.c_uint64),
    ("docs_evaluated", ct.c_uint64),
    ("postings_decoded", ct.c_uint64),
]


class ZeSealRequest(ct.Structure):
    _fields_ = [
        ("abi_size", ct.c_uint32),
        ("abi_reserved", ct.c_uint32),
        ("cancel_token", ct.c_uint64),
    ]


class ZeGenerationReport(ct.Structure):
    _fields_ = [
        ("abi_size", ct.c_uint32),
        ("abi_reserved", ct.c_uint32),
        ("generation", ct.c_uint64),
    ]


class ZeDropPartitionRequest(ct.Structure):
    _fields_ = [
        ("abi_size", ct.c_uint32),
        ("abi_reserved", ct.c_uint32),
        ("start_ts", ct.c_int64),
        ("end_ts", ct.c_int64),
    ]


class ZePartitionReport(ct.Structure):
    _fields_ = [
        ("abi_size", ct.c_uint32),
        ("abi_reserved", ct.c_uint32),
        ("generation", ct.c_uint64),
        ("segments_dropped", ct.c_uint64),
        ("bytes_reclaimed", ct.c_uint64),
        ("straddlers_skipped", ct.c_uint64),
        ("is_no_op", ct.c_uint32),
        ("reserved", ct.c_uint32),
    ]


class ZeRetentionRequest(ct.Structure):
    _fields_ = [
        ("abi_size", ct.c_uint32),
        ("abi_reserved", ct.c_uint32),
        ("window", ct.c_int64),
        ("now_ts", ct.c_int64),
    ]


class ZePurgeRequest(ct.Structure):
    _fields_ = [
        ("abi_size", ct.c_uint32),
        ("abi_reserved", ct.c_uint32),
        ("doc_ids", ct.POINTER(ZeDocId)),
        ("doc_id_count", ct.c_size_t),
    ]


class ZePurgeTokenReport(ct.Structure):
    _fields_ = [
        ("abi_size", ct.c_uint32),
        ("abi_reserved", ct.c_uint32),
        ("token_id", ct.c_uint64),
        ("generation", ct.c_uint64),
        ("unknown_id_count", ct.c_uint64),
        ("is_no_op", ct.c_uint32),
        ("reserved", ct.c_uint32),
    ]


class ZeAwaitPurgeRequest(ct.Structure):
    _fields_ = [
        ("abi_size", ct.c_uint32),
        ("abi_reserved", ct.c_uint32),
        ("token_id", ct.c_uint64),
    ]


class ZePurgeReport(ct.Structure):
    _fields_ = [
        ("abi_size", ct.c_uint32),
        ("abi_reserved", ct.c_uint32),
        ("generation", ct.c_uint64),
        ("segments_rewritten", ct.c_uint64),
        ("unknown_id_count", ct.c_uint64),
        ("wal_rewritten", ct.c_uint32),
        ("is_no_op", ct.c_uint32),
    ]


class ZeMaintainRequest(ct.Structure):
    _fields_ = [
        ("abi_size", ct.c_uint32),
        ("abi_reserved", ct.c_uint32),
        ("wall_time_ns", ct.c_uint64),
        ("bytes", ct.c_uint64),
    ]


class ZeMaintainReport(ct.Structure):
    _fields_ = [
        ("abi_size", ct.c_uint32),
        ("abi_reserved", ct.c_uint32),
        ("graphs_built", ct.c_uint64),
        ("bytes_consumed", ct.c_uint64),
        ("checkpoints_resumed", ct.c_uint64),
        ("status", ct.c_int32),
        ("reserved", ct.c_uint32),
    ]


SizedStructure = TypeVar("SizedStructure", bound=ct.Structure)


def sized(structure_type: type[SizedStructure]) -> SizedStructure:
    """Return a zeroed ABI structure with its frozen size initialized."""

    value = structure_type()
    value.abi_size = ct.sizeof(structure_type)
    return value
