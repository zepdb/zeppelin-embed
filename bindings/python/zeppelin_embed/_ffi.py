"""Function signatures for the generated Zeppelin Embed C header."""

from __future__ import annotations

import ctypes as ct

from . import _structures as s
from ._library import LIBRARY

Status = ct.c_int32
Handle = ct.c_uint64
CancelHandle = ct.c_uint64


def _bind(name: str, arguments: list[object], result: object = Status) -> None:
    function = getattr(LIBRARY, name)
    function.argtypes = arguments
    function.restype = result


_bind("ze_abi_version", [], ct.c_uint32)
_bind("ze_open", [ct.POINTER(s.ZeOpenRequest), ct.POINTER(Handle)])
_bind(
    "ze_namespace_open",
    [ct.POINTER(s.ZeNamespaceOpenRequest), ct.POINTER(Handle)],
)
_bind(
    "ze_namespace_list",
    [ct.POINTER(s.ZeNamespaceListRequest), ct.POINTER(s.ZeNamespaceListResult)],
)
_bind("ze_namespace_list_result_free", [ct.POINTER(s.ZeNamespaceListResult)])
_bind(
    "ze_open_with_epoch",
    [ct.POINTER(s.ZeOpenRequest), ct.POINTER(s.ZeEpochRequest), ct.POINTER(Handle)],
)
_bind("ze_close", [Handle])
_bind("ze_state", [Handle, ct.POINTER(s.ZeStateReport)])
_bind("ze_stats", [Handle, ct.POINTER(s.ZeStatsReport)])
_bind("ze_ingest", [Handle, ct.POINTER(s.ZeIngestRequest), ct.POINTER(s.ZeMutationReport)])
_bind("ze_upsert", [Handle, ct.POINTER(s.ZeUpsertRequest), ct.POINTER(s.ZeMutationReport)])
_bind("ze_get", [Handle, ct.POINTER(s.ZeGetRequest), ct.POINTER(s.ZeGetResult)])
_bind("ze_get_result_free", [ct.POINTER(s.ZeGetResult)])
_bind("ze_delete", [Handle, ct.POINTER(s.ZeDeleteRequest), ct.POINTER(s.ZeMutationReport)])
_bind("ze_scan", [Handle, ct.POINTER(s.ZeScanRequest), ct.POINTER(s.ZeScanResult)])
_bind("ze_scan_result_free", [ct.POINTER(s.ZeScanResult)])
_bind("ze_count", [Handle, ct.POINTER(s.ZeCountRequest), ct.POINTER(s.ZeCountResult)])
_bind("ze_search", [Handle, ct.POINTER(s.ZeSearchRequest), ct.POINTER(s.ZeSearchResult)])
_bind(
    "ze_search_filtered",
    [Handle, ct.POINTER(s.ZeSearchFilteredRequest), ct.POINTER(s.ZeSearchResult)],
)
_bind("ze_search_result_free", [ct.POINTER(s.ZeSearchResult)])
_bind("ze_query", [Handle, ct.POINTER(s.ZeQueryRequest), ct.POINTER(s.ZeQueryResult)])
_bind("ze_query_result_free", [ct.POINTER(s.ZeQueryResult)])
_bind("ze_seal", [Handle, ct.POINTER(s.ZeSealRequest), ct.POINTER(s.ZeGenerationReport)])
_bind(
    "ze_drop_partition",
    [Handle, ct.POINTER(s.ZeDropPartitionRequest), ct.POINTER(s.ZePartitionReport)],
)
_bind(
    "ze_apply_retention",
    [Handle, ct.POINTER(s.ZeRetentionRequest), ct.POINTER(s.ZePartitionReport)],
)
_bind("ze_purge", [Handle, ct.POINTER(s.ZePurgeRequest), ct.POINTER(s.ZePurgeTokenReport)])
_bind(
    "ze_await_physical_purge",
    [Handle, ct.POINTER(s.ZeAwaitPurgeRequest), ct.POINTER(s.ZePurgeReport)],
)
_bind(
    "ze_maintain",
    [Handle, ct.POINTER(s.ZeMaintainRequest), ct.POINTER(s.ZeMaintainReport)],
)
_bind("ze_epoch_identity", [ct.POINTER(s.ZeEpochRequest), ct.POINTER(s.ZeEpochIdentity)])
_bind("ze_epoch_current", [Handle, ct.POINTER(s.ZeEpochIdentity)])
_bind(
    "ze_epoch_switch_alias",
    [Handle, ct.POINTER(s.ZeEpochRequest), ct.POINTER(s.ZeEpochAliasReport)],
)
_bind(
    "ze_epoch_drop",
    [Handle, ct.POINTER(s.ZeEpochRequest), ct.POINTER(s.ZeEpochDropReport)],
)
_bind("ze_cancel_token_create", [ct.POINTER(CancelHandle)])
_bind("ze_cancel_token_cancel", [CancelHandle])
_bind("ze_cancel_token_free", [CancelHandle])

TEXT_AVAILABLE = hasattr(LIBRARY, "ze_text_open")
if TEXT_AVAILABLE:
    _bind("ze_text_open", [ct.POINTER(s.ZeTextOpenRequest), ct.POINTER(Handle)])
    _bind(
        "ze_text_ingest",
        [Handle, ct.POINTER(s.ZeTextIngestRequest), ct.POINTER(s.ZeMutationReport)],
    )
    _bind(
        "ze_text_query",
        [Handle, ct.POINTER(s.ZeTextQueryRequest), ct.POINTER(s.ZeTextQueryResult)],
    )
    _bind("ze_text_query_result_free", [ct.POINTER(s.ZeTextQueryResult)])


ABI_VERSION = int(LIBRARY.ze_abi_version())
if ABI_VERSION != 1:
    raise ImportError(f"unsupported Zeppelin Embed ABI version {ABI_VERSION}; expected 1")
