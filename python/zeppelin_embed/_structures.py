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
        ("reserved", ct.c_uint32),
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
