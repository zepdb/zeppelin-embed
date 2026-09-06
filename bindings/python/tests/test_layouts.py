"""Frozen ABI layout checks copied from Rust's ``ffi_contract.rs``."""

from __future__ import annotations

import ctypes as ct

from zeppelin_embed import _structures as s

LAYOUTS: dict[str, tuple[int, int, dict[str, int]]] = {
    "ZeOpenRequest": (64, 8, {"abi_size": 0, "abi_reserved": 4, "path": 8, "path_len": 16, "access_mode": 24, "durability_mode": 28, "commit_tier": 32, "reader_drain_timeout_ms": 40, "max_resident_bytes": 48, "max_temp_bytes": 56}),
    "ZeStateReport": (16, 4, {"abi_size": 0, "abi_reserved": 4, "state": 8, "reserved": 12}),
    "ZeStatsReport": (144, 8, {"abi_size": 0, "abi_reserved": 4, "resident_owned_bytes": 8, "mapped_bytes": 16, "mapped_resident_bytes": 24, "segment_bytes": 32, "active_segment_bytes": 40, "active_row_count": 48, "tombstone_count": 56, "tombstone_bytes": 64, "wal_bytes": 72, "cache_bytes": 80, "temporary_bytes": 88, "query_pool_bytes": 96, "open_files": 104, "active_queries": 112, "active_snapshot_leases": 120, "phys_footprint": 128, "has_phys_footprint": 136, "reserved": 140}),
    "ZeDocId": (16, 8, {"high": 0, "low": 8}),
    "ZeIngestDocument": (88, 8, {"abi_size": 0, "abi_reserved": 4, "doc_id": 8, "revision": 24, "timestamp": 32, "vector": 40, "vector_len": 48, "metadata": 56, "metadata_len": 64, "text": 72, "text_len": 80}),
    "ZeIngestRequest": (32, 8, {"abi_size": 0, "abi_reserved": 4, "documents": 8, "document_count": 16, "dimension": 24}),
    "ZeDeleteRequest": (24, 8, {"abi_size": 0, "abi_reserved": 4, "doc_ids": 8, "doc_id_count": 16}),
    "ZeMutationReport": (24, 8, {"abi_size": 0, "abi_reserved": 4, "sequence": 8, "generation": 16}),
    "ZeSearchRequest": (96, 8, {"abi_size": 0, "abi_reserved": 4, "vector": 8, "vector_len": 16, "dimension": 24, "k": 32, "thread_budget": 40, "has_tier": 48, "tier": 52, "graph_profile": 56, "reserved": 60, "graph_ef": 64, "graph_seed": 72, "cancel_token": 80, "deadline_ns": 88}),
    "ZeSearchHit": (64, 8, {"source_kind": 0, "reserved": 4, "segment_id": 8, "local_row": 24, "has_document": 28, "doc_id": 32, "revision": 48, "score": 56, "reserved_tail": 60}),
    "ZeSearchResult": (112, 8, {"abi_size": 0, "abi_reserved": 4, "hits": 8, "hit_count": 16, "generation": 24, "dims_touched": 32, "bytes_read": 40, "threads_used": 48, "graph_segments_traversed": 56, "graph_validations": 64, "graph_entry_seed_discoveries": 72, "graph_visited_epoch_clears": 80, "graph_candidates_scored": 88, "graph_candidates_rescored": 96, "graph_segments_pruned_by_bound": 104}),
    "ZeSealRequest": (16, 8, {"abi_size": 0, "abi_reserved": 4, "cancel_token": 8}),
    "ZeGenerationReport": (16, 8, {"abi_size": 0, "abi_reserved": 4, "generation": 8}),
    "ZeDropPartitionRequest": (24, 8, {"abi_size": 0, "abi_reserved": 4, "start_ts": 8, "end_ts": 16}),
    "ZeRetentionRequest": (24, 8, {"abi_size": 0, "abi_reserved": 4, "window": 8, "now_ts": 16}),
    "ZePartitionReport": (48, 8, {"abi_size": 0, "abi_reserved": 4, "generation": 8, "segments_dropped": 16, "bytes_reclaimed": 24, "straddlers_skipped": 32, "is_no_op": 40, "reserved": 44}),
    "ZePurgeRequest": (24, 8, {"abi_size": 0, "abi_reserved": 4, "doc_ids": 8, "doc_id_count": 16}),
    "ZePurgeTokenReport": (40, 8, {"abi_size": 0, "abi_reserved": 4, "token_id": 8, "generation": 16, "unknown_id_count": 24, "is_no_op": 32, "reserved": 36}),
    "ZeAwaitPurgeRequest": (16, 8, {"abi_size": 0, "abi_reserved": 4, "token_id": 8}),
    "ZePurgeReport": (40, 8, {"abi_size": 0, "abi_reserved": 4, "generation": 8, "segments_rewritten": 16, "unknown_id_count": 24, "wal_rewritten": 32, "is_no_op": 36}),
    "ZeMaintainRequest": (24, 8, {"abi_size": 0, "abi_reserved": 4, "wall_time_ns": 8, "bytes": 16}),
    "ZeMaintainReport": (40, 8, {"abi_size": 0, "abi_reserved": 4, "graphs_built": 8, "bytes_consumed": 16, "checkpoints_resumed": 24, "status": 32, "reserved": 36}),
    "ZeEmbeddingTower": (104, 8, {"model_id": 0, "model_id_len": 8, "model_version": 16, "model_version_len": 24, "weights_digest": 32, "weights_digest_len": 40, "dims": 48, "normalization": 52, "prompt_prefix": 56, "prompt_prefix_len": 64, "max_tokens": 72, "runtime": 76, "compute_units": 80, "has_os_build": 84, "os_build": 88, "os_build_len": 96}),
    "ZeEmbeddingEpoch": (224, 8, {"document": 0, "query": 104, "alignment_digest": 208, "alignment_digest_len": 216}),
    "ZeEpochRequest": (240, 8, {"abi_size": 0, "abi_reserved": 4, "embedding": 8, "tokenizer_profile": 232, "reserved": 236}),
    "ZeEpochIdentity": (24, 8, {"abi_size": 0, "abi_reserved": 4, "embedding_epoch": 8, "tokenizer_epoch": 16}),
    "ZeEpochAliasReport": (56, 8, {"abi_size": 0, "abi_reserved": 4, "generation": 8, "previous_embedding_epoch": 16, "previous_tokenizer_epoch": 24, "published_embedding_epoch": 32, "published_tokenizer_epoch": 40, "manifest_committed": 48, "reserved": 52}),
    "ZeEpochDropReport": (32, 8, {"abi_size": 0, "abi_reserved": 4, "generation": 8, "segments_dropped": 16, "bytes_reclaimed": 24}),
    "ZeAttributeDefinition": (32, 8, {"attribute_id": 0, "name": 8, "name_len": 16, "attribute_type": 24, "nullable": 28}),
    "ZeNamespaceSpec": (48, 8, {"abi_size": 0, "abi_reserved": 4, "attributes": 8, "attribute_count": 16, "has_vector_space": 24, "dimensions": 28, "normalization": 32, "epoch": 40}),
    "ZeNamespaceOpenRequest": (112, 8, {"abi_size": 0, "abi_reserved": 4, "root": 8, "root_len": 16, "name": 24, "name_len": 32, "open": 40, "spec": 104}),
    "ZeNamespaceListRequest": (24, 8, {"abi_size": 0, "abi_reserved": 4, "root": 8, "root_len": 16}),
    "ZeNamespaceEntry": (16, 8, {"name": 0, "name_len": 8}),
    "ZeNamespaceListResult": (24, 8, {"abi_size": 0, "abi_reserved": 4, "entries": 8, "entry_count": 16}),
    "ZeAttributeValue": (56, 8, {"attribute_id": 0, "value_type": 4, "u64_value": 8, "i64_value": 16, "f64_value": 24, "bool_value": 32, "string_value": 40, "string_len": 48}),
    "ZeUpsertDocument": (112, 8, {"abi_size": 0, "abi_reserved": 4, "document": 8, "attributes": 96, "attribute_count": 104}),
    "ZeUpsertRequest": (32, 8, {"abi_size": 0, "abi_reserved": 4, "documents": 8, "document_count": 16, "dimension": 24}),
    "ZeGetRequest": (40, 8, {"abi_size": 0, "abi_reserved": 4, "ids": 8, "id_count": 16, "include_vector": 24, "include_text": 28, "include_metadata": 32, "include_attributes": 36}),
    "ZeStoredDocument": (104, 8, {"has_document": 0, "doc_id": 8, "revision": 24, "timestamp": 32, "vector": 40, "vector_len": 48, "text": 56, "text_len": 64, "metadata": 72, "metadata_len": 80, "attributes": 88, "attribute_count": 96}),
    "ZeGetResult": (40, 8, {"abi_size": 0, "abi_reserved": 4, "documents": 8, "document_count": 16, "missing_count": 24, "generation": 32}),
    "ZeFilterNode": (168, 8, {"op": 0, "attribute_id": 4, "values": 8, "value_count": 16, "has_lower": 24, "lower": 32, "lower_inclusive": 88, "has_upper": 92, "upper": 96, "upper_inclusive": 152, "children_start": 156, "children_count": 160}),
    "ZeFilter": (32, 8, {"abi_size": 0, "abi_reserved": 4, "nodes": 8, "node_count": 16, "root": 24}),
    "ZeScanRequest": (112, 8, {"abi_size": 0, "abi_reserved": 4, "cursor_generation": 8, "cursor_segment_id": 16, "cursor_next_row": 32, "cursor_phase": 36, "limit": 40, "order": 48, "include_vector": 52, "include_text": 56, "include_metadata": 60, "include_attributes": 64, "has_timestamp_range": 68, "start_ts": 72, "end_ts": 80, "filter": 88, "cancel_token": 96, "deadline_ns": 104}),
    "ZeScanResult": (64, 8, {"abi_size": 0, "abi_reserved": 4, "documents": 8, "document_count": 16, "generation": 24, "has_more": 32, "next_segment_id": 36, "next_row": 52, "next_phase": 56}),
    "ZeCountRequest": (40, 8, {"abi_size": 0, "abi_reserved": 4, "filter": 8, "has_timestamp_range": 16, "start_ts": 24, "end_ts": 32}),
    "ZeCountResult": (24, 8, {"abi_size": 0, "abi_reserved": 4, "count": 8, "generation": 16}),
    "ZeSearchFilteredRequest": (112, 8, {"abi_size": 0, "abi_reserved": 4, "search": 8, "filter": 104}),
    "ZeQueryRequest": (160, 8, {"abi_size": 0, "abi_reserved": 4, "vector": 8, "vector_len": 16, "dimension": 24, "text": 32, "text_len": 40, "k": 48, "thread_budget": 56, "has_tier": 64, "tier": 68, "graph_profile": 72, "reserved": 76, "graph_ef": 80, "graph_seed": 88, "has_alpha": 96, "rules_enabled": 100, "alpha": 104, "has_max_rounds": 112, "quoted_phrase": 116, "max_rounds": 120, "identifier_token": 128, "has_rarest_exact_document_frequency": 132, "rarest_exact_document_frequency": 136, "cancel_token": 144, "deadline_ns": 152}),
    "ZeQueryHit": (64, 8, {"has_document": 0, "has_revision": 4, "doc_id": 8, "revision": 24, "score": 32, "has_vector_score": 40, "has_lexical_score": 44, "vector_squared_l2": 48, "lexical_bm25": 56}),
    "ZeQueryResult": (128, 8, {"abi_size": 0, "abi_reserved": 4, "hits": 8, "hit_count": 16, "generation": 24, "mode": 32, "approximate": 36, "exact_rescore": 40, "budget_exhausted": 44, "has_fusion": 48, "fusion_method": 52, "effective_alpha": 56, "fusion_rounds": 64, "has_embedding_epoch": 72, "has_tokenizer_epoch": 76, "embedding_epoch": 80, "tokenizer_epoch": 88, "dims_touched": 96, "bytes_read": 104, "docs_evaluated": 112, "postings_decoded": 120}),
}


def test_ctypes_struct_layouts_match_rust_goldens() -> None:
    for name, (expected_size, expected_alignment, expected_offsets) in LAYOUTS.items():
        structure = getattr(s, name)
        assert ct.sizeof(structure) == expected_size, f"{name} size"
        assert ct.alignment(structure) == expected_alignment, f"{name} alignment"
        for field, expected_offset in expected_offsets.items():
            actual_offset = getattr(structure, field).offset
            assert actual_offset == expected_offset, f"{name}.{field} offset"
