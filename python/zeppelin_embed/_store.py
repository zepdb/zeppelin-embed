"""Pythonic Store wrapper over the frozen Zeppelin Embed C ABI."""

from __future__ import annotations

import ctypes as ct
import os
from collections.abc import Sequence
from enum import IntEnum
from pathlib import Path
from typing import Any, Self, TypeVar

import numpy as np

from . import _structures as s
from ._errors import ERROR_TYPES, invalid_argument, last_error_message, raise_for_status
from ._library import LIBRARY
from ._types import (
    AccessMode,
    CommitTier,
    ComputeUnits,
    DurabilityMode,
    EmbeddingEpoch,
    EmbeddingRuntime,
    EpochAliasReport,
    EpochDropReport,
    EpochIdentity,
    FusionMethod,
    FusionReport,
    GenerationReport,
    GraphProfile,
    MaintainReport,
    MaintenanceStatus,
    MutationReport,
    Normalization,
    PartitionReport,
    PurgeReport,
    PurgeTokenReport,
    QueryHit,
    QueryMode,
    QueryResult,
    SearchHit,
    SearchResult,
    StateReport,
    StatsReport,
    StoreState,
    Tier,
)

EnumValue = TypeVar("EnumValue", bound=IntEnum)


def _enum_value(value: EnumValue | str, enum_type: type[EnumValue], field: str) -> EnumValue:
    if isinstance(value, enum_type):
        return value
    if isinstance(value, str):
        normalized = value.upper().replace("-", "_")
        try:
            return enum_type[normalized]
        except KeyError:
            invalid_argument(f"{field} value {value!r} is not supported")
    invalid_argument(f"{field} must be a {enum_type.__name__} or string")


def _bytes_pointer(data: bytes) -> tuple[Any, object | None]:
    if not data:
        return s.UInt8Pointer(), None
    owner = (ct.c_uint8 * len(data)).from_buffer_copy(data)
    return ct.cast(owner, s.UInt8Pointer), owner


def _text_pointer(text: str) -> tuple[Any, int, object | None]:
    try:
        encoded = text.encode("utf-8", errors="strict")
    except UnicodeEncodeError as error:
        invalid_argument(f"text is not valid UTF-8: {error}")
    pointer, owner = _bytes_pointer(encoded)
    return pointer, len(encoded), owner


def _doc_id(value: int | tuple[int, int]) -> s.ZeDocId:
    if isinstance(value, tuple):
        if len(value) != 2:
            invalid_argument("document-id tuple must contain high and low u64 values")
        high, low = value
    elif isinstance(value, int):
        if value < 0 or value >= 1 << 128:
            invalid_argument("document id must fit an unsigned 128-bit integer")
        high, low = value >> 64, value & ((1 << 64) - 1)
    else:
        invalid_argument("document id must be an integer or (high, low) tuple")
    if not isinstance(high, int) or not isinstance(low, int):
        invalid_argument("document-id halves must be integers")
    if high < 0 or high >= 1 << 64 or low < 0 or low >= 1 << 64:
        invalid_argument("document-id halves must fit unsigned 64-bit integers")
    return s.ZeDocId(high=high, low=low)


def _python_doc_id(value: s.ZeDocId) -> int:
    return (int(value.high) << 64) | int(value.low)


def _vector(value: np.ndarray[Any, Any], *, dimensions: int) -> np.ndarray[Any, Any]:
    if not isinstance(value, np.ndarray):
        invalid_argument("vector must be a NumPy array")
    if value.dtype != np.dtype(np.float32):
        invalid_argument("vector dtype must be float32")
    if value.ndim != dimensions:
        invalid_argument(f"vector must have exactly {dimensions} dimension(s)")
    if not value.flags.c_contiguous:
        invalid_argument("vector must be C-contiguous")
    if int(value.ctypes.data) % ct.alignment(ct.c_float) != 0:
        invalid_argument("vector buffer must be aligned for float32")
    return value


def _same_length(name: str, values: Sequence[object], expected: int) -> None:
    if len(values) != expected:
        invalid_argument(f"{name} length {len(values)} does not match document count {expected}")


def _tower(value: Any, owners: list[object]) -> s.ZeEmbeddingTower:
    model_id, model_id_len, owner = _text_pointer(value.model_id)
    if owner is not None:
        owners.append(owner)
    model_version, model_version_len, owner = _text_pointer(value.model_version)
    if owner is not None:
        owners.append(owner)
    weights, owner = _bytes_pointer(value.weights_digest)
    if owner is not None:
        owners.append(owner)
    prompt, prompt_len, owner = _text_pointer(value.prompt_prefix)
    if owner is not None:
        owners.append(owner)
    if value.os_build is None:
        os_build, os_build_len, has_os_build = s.UInt8Pointer(), 0, 0
    else:
        os_build, os_build_len, owner = _text_pointer(value.os_build)
        if owner is not None:
            owners.append(owner)
        has_os_build = 1
    return s.ZeEmbeddingTower(
        model_id=model_id,
        model_id_len=model_id_len,
        model_version=model_version,
        model_version_len=model_version_len,
        weights_digest=weights,
        weights_digest_len=len(value.weights_digest),
        dims=value.dims,
        normalization=int(_enum_value(value.normalization, Normalization, "normalization")),
        prompt_prefix=prompt,
        prompt_prefix_len=prompt_len,
        max_tokens=value.max_tokens,
        runtime=int(_enum_value(value.runtime, EmbeddingRuntime, "runtime")),
        compute_units=int(_enum_value(value.compute_units, ComputeUnits, "compute_units")),
        has_os_build=has_os_build,
        os_build=os_build,
        os_build_len=os_build_len,
    )


def _epoch_request(value: EmbeddingEpoch) -> tuple[s.ZeEpochRequest, list[object]]:
    owners: list[object] = []
    document = _tower(value.document, owners)
    query = _tower(value.query, owners)
    alignment, owner = _bytes_pointer(value.alignment_digest)
    if owner is not None:
        owners.append(owner)
    request = s.ZeEpochRequest(
        abi_size=ct.sizeof(s.ZeEpochRequest),
        abi_reserved=0,
        embedding=s.ZeEmbeddingEpoch(
            document=document,
            query=query,
            alignment_digest=alignment,
            alignment_digest_len=len(value.alignment_digest),
        ),
        tokenizer_profile=value.tokenizer_profile,
        reserved=0,
    )
    return request, owners


def _open_request(
    path: str | os.PathLike[str],
    access_mode: AccessMode | str,
    durability: DurabilityMode | str,
    commit_tier: CommitTier | str,
    reader_drain_timeout_ms: int,
    max_resident_bytes: int,
    max_temp_bytes: int,
) -> tuple[s.ZeOpenRequest, object]:
    encoded = os.fspath(path).encode("utf-8", errors="strict")
    pointer, owner = _bytes_pointer(encoded)
    if owner is None:
        invalid_argument("store path must not be empty")
    request = s.ZeOpenRequest(
        abi_size=ct.sizeof(s.ZeOpenRequest),
        abi_reserved=0,
        path=pointer,
        path_len=len(encoded),
        access_mode=int(_enum_value(access_mode, AccessMode, "access_mode")),
        durability_mode=int(_enum_value(durability, DurabilityMode, "durability")),
        commit_tier=int(_enum_value(commit_tier, CommitTier, "commit_tier")),
        reader_drain_timeout_ms=reader_drain_timeout_ms,
        max_resident_bytes=max_resident_bytes,
        max_temp_bytes=max_temp_bytes,
    )
    return request, owner


def epoch_identity(epoch: EmbeddingEpoch) -> EpochIdentity:
    """Compute the compact ABI identity of an epoch declaration."""

    request, owners = _epoch_request(epoch)
    report = s.sized(s.ZeEpochIdentity)
    status = LIBRARY.ze_epoch_identity(ct.byref(request), ct.byref(report))
    del owners
    raise_for_status(status)
    return EpochIdentity(int(report.embedding_epoch), int(report.tokenizer_epoch))


class CancelToken:
    """Generation-tagged cooperative cancellation token."""

    def __init__(self) -> None:
        token = ct.c_uint64()
        raise_for_status(LIBRARY.ze_cancel_token_create(ct.byref(token)))
        self._value = int(token.value)
        self._cancelled = False

    @property
    def value(self) -> int:
        if self._value == 0:
            raise ERROR_TYPES[3]("cancellation token is closed")
        return self._value

    @property
    def cancelled(self) -> bool:
        return self._cancelled

    @property
    def closed(self) -> bool:
        return self._value == 0

    def cancel(self) -> None:
        value = self.value
        raise_for_status(LIBRARY.ze_cancel_token_cancel(value))
        self._cancelled = True

    def close(self) -> None:
        if self._value == 0:
            return
        value = self._value
        self._value = 0
        raise_for_status(LIBRARY.ze_cancel_token_free(value))

    def __enter__(self) -> Self:
        return self

    def __exit__(self, _type: object, _value: object, _traceback: object) -> None:
        self.close()


class Store:
    """One deterministic wrapper around a generation-tagged store handle."""

    def __init__(self, handle: int) -> None:
        self._handle = handle
        self._abi_call_count = 0

    @property
    def closed(self) -> bool:
        return self._handle == 0

    @property
    def abi_call_count(self) -> int:
        """Number of store ABI calls, exposed for validation-boundary tests."""

        return self._abi_call_count

    def _live_handle(self) -> int:
        if self._handle == 0:
            raise ERROR_TYPES[3]("store is closed")
        return self._handle

    def _call(self, function: Any, *arguments: object) -> int:
        self._abi_call_count += 1
        return int(function(*arguments))

    def close(self) -> None:
        if self._handle == 0:
            return
        handle = self._handle
        status = self._call(LIBRARY.ze_close, handle)
        self._handle = 0
        raise_for_status(status, handle)

    def __enter__(self) -> Self:
        self._live_handle()
        return self

    def __exit__(self, _type: object, _value: object, _traceback: object) -> None:
        self.close()

    def state(self) -> StateReport:
        handle = self._live_handle()
        report = s.sized(s.ZeStateReport)
        raise_for_status(self._call(LIBRARY.ze_state, handle, ct.byref(report)), handle)
        return StateReport(StoreState(report.state))

    def stats(self) -> StatsReport:
        handle = self._live_handle()
        report = s.sized(s.ZeStatsReport)
        raise_for_status(self._call(LIBRARY.ze_stats, handle, ct.byref(report)), handle)
        return StatsReport(
            resident_owned_bytes=int(report.resident_owned_bytes),
            mapped_bytes=int(report.mapped_bytes),
            mapped_resident_bytes=int(report.mapped_resident_bytes),
            segment_bytes=int(report.segment_bytes),
            active_segment_bytes=int(report.active_segment_bytes),
            active_row_count=int(report.active_row_count),
            tombstone_count=int(report.tombstone_count),
            tombstone_bytes=int(report.tombstone_bytes),
            wal_bytes=int(report.wal_bytes),
            cache_bytes=int(report.cache_bytes),
            temporary_bytes=int(report.temporary_bytes),
            query_pool_bytes=int(report.query_pool_bytes),
            open_files=int(report.open_files),
            active_queries=int(report.active_queries),
            active_snapshot_leases=int(report.active_snapshot_leases),
            phys_footprint=int(report.phys_footprint) if report.has_phys_footprint else None,
        )

    def ingest(
        self,
        doc_ids: Sequence[int | tuple[int, int]],
        vectors: np.ndarray[Any, Any],
        *,
        revisions: Sequence[int] | None = None,
        timestamps: Sequence[int] | None = None,
        texts: Sequence[str | None] | None = None,
        metadata: Sequence[bytes | None] | None = None,
        batch_size: int = 1_024,
    ) -> MutationReport:
        handle = self._live_handle()
        matrix = _vector(vectors, dimensions=2)
        count, dimension = matrix.shape
        _same_length("doc_ids", doc_ids, count)
        revisions = [1] * count if revisions is None else revisions
        timestamps = list(range(count)) if timestamps is None else timestamps
        texts = [None] * count if texts is None else texts
        metadata = [None] * count if metadata is None else metadata
        for name, values in (
            ("revisions", revisions),
            ("timestamps", timestamps),
            ("texts", texts),
            ("metadata", metadata),
        ):
            _same_length(name, values, count)
        if batch_size <= 0:
            invalid_argument("batch_size must be positive")
        last = MutationReport(0, 0)
        starts = range(0, count, batch_size) if count else (0,)
        for start in starts:
            end = min(start + batch_size, count)
            length = end - start
            documents = (s.ZeIngestDocument * length)()
            owners: list[object] = [matrix, documents]
            for local, index in enumerate(range(start, end)):
                text = texts[index]
                if text is None:
                    text_pointer, text_len = s.UInt8Pointer(), 0
                elif isinstance(text, str):
                    text_pointer, text_len, owner = _text_pointer(text)
                    if owner is not None:
                        owners.append(owner)
                else:
                    invalid_argument("texts values must be str or None")
                payload = metadata[index]
                if payload is None:
                    metadata_pointer, metadata_len = s.UInt8Pointer(), 0
                elif isinstance(payload, bytes):
                    metadata_pointer, owner = _bytes_pointer(payload)
                    metadata_len = len(payload)
                    if owner is not None:
                        owners.append(owner)
                else:
                    invalid_argument("metadata values must be bytes or None")
                row = matrix[index]
                documents[local] = s.ZeIngestDocument(
                    abi_size=ct.sizeof(s.ZeIngestDocument),
                    abi_reserved=0,
                    doc_id=_doc_id(doc_ids[index]),
                    revision=revisions[index],
                    timestamp=timestamps[index],
                    vector=row.ctypes.data_as(s.FloatPointer),
                    vector_len=dimension,
                    metadata=metadata_pointer,
                    metadata_len=metadata_len,
                    text=text_pointer,
                    text_len=text_len,
                )
            request = s.ZeIngestRequest(
                abi_size=ct.sizeof(s.ZeIngestRequest),
                abi_reserved=0,
                documents=ct.cast(documents, ct.POINTER(s.ZeIngestDocument)),
                document_count=length,
                dimension=dimension,
            )
            report = s.sized(s.ZeMutationReport)
            status = self._call(LIBRARY.ze_ingest, handle, ct.byref(request), ct.byref(report))
            del owners
            raise_for_status(status, handle)
            last = MutationReport(int(report.sequence), int(report.generation))
        return last

    def delete(self, doc_ids: Sequence[int | tuple[int, int]]) -> MutationReport:
        handle = self._live_handle()
        ids = (s.ZeDocId * len(doc_ids))(*[_doc_id(value) for value in doc_ids])
        request = s.ZeDeleteRequest(
            abi_size=ct.sizeof(s.ZeDeleteRequest),
            abi_reserved=0,
            doc_ids=ct.cast(ids, ct.POINTER(s.ZeDocId)),
            doc_id_count=len(ids),
        )
        report = s.sized(s.ZeMutationReport)
        raise_for_status(
            self._call(LIBRARY.ze_delete, handle, ct.byref(request), ct.byref(report)),
            handle,
        )
        return MutationReport(int(report.sequence), int(report.generation))

    def search(
        self,
        vector: np.ndarray[Any, Any],
        *,
        k: int = 10,
        thread_budget: int = 0,
        tier: Tier | None = None,
        graph_profile: GraphProfile = GraphProfile.SIFT_CLASS,
        graph_ef: int = 0,
        graph_seed: int = 0,
        cancel_token: CancelToken | None = None,
        deadline_ns: int = 0,
    ) -> SearchResult:
        handle = self._live_handle()
        probe = _vector(vector, dimensions=1)
        request = s.ZeSearchRequest(
            abi_size=ct.sizeof(s.ZeSearchRequest),
            abi_reserved=0,
            vector=probe.ctypes.data_as(s.FloatPointer),
            vector_len=probe.size,
            dimension=probe.size,
            k=k,
            thread_budget=thread_budget,
            has_tier=0 if tier is None else 1,
            tier=0 if tier is None else int(_enum_value(tier, Tier, "tier")),
            graph_profile=int(_enum_value(graph_profile, GraphProfile, "graph_profile")),
            reserved=0,
            graph_ef=graph_ef,
            graph_seed=graph_seed,
            cancel_token=0 if cancel_token is None else cancel_token.value,
            deadline_ns=deadline_ns,
        )
        result = s.sized(s.ZeSearchResult)
        primary: BaseException | None = None
        try:
            status = self._call(LIBRARY.ze_search, handle, ct.byref(request), ct.byref(result))
            raise_for_status(status, handle)
            hits = tuple(
                SearchHit(
                    source_kind=int(hit.source_kind),
                    segment_id=bytes(hit.segment_id),
                    local_row=int(hit.local_row),
                    doc_id=_python_doc_id(hit.doc_id) if hit.has_document else None,
                    revision=int(hit.revision) if hit.has_document else None,
                    score=float(hit.score),
                )
                for hit in (result.hits[index] for index in range(result.hit_count))
            )
            return SearchResult(
                hits=hits,
                generation=int(result.generation),
                dims_touched=int(result.dims_touched),
                bytes_read=int(result.bytes_read),
                threads_used=int(result.threads_used),
                graph_segments_traversed=int(result.graph_segments_traversed),
                graph_validations=int(result.graph_validations),
                graph_entry_seed_discoveries=int(result.graph_entry_seed_discoveries),
                graph_visited_epoch_clears=int(result.graph_visited_epoch_clears),
                graph_candidates_scored=int(result.graph_candidates_scored),
                graph_candidates_rescored=int(result.graph_candidates_rescored),
                graph_segments_pruned_by_bound=int(result.graph_segments_pruned_by_bound),
            )
        except BaseException as error:
            primary = error
            raise
        finally:
            free_status = self._call(LIBRARY.ze_search_result_free, ct.byref(result))
            if free_status != 0:
                if primary is None:
                    raise_for_status(free_status, handle)
                assert primary is not None
                primary.add_note(f"ze_search_result_free: {last_error_message(handle)}")

    def query(
        self,
        *,
        vector: np.ndarray[Any, Any] | None = None,
        text: str | None = None,
        k: int = 10,
        thread_budget: int = 0,
        tier: Tier | None = None,
        graph_profile: GraphProfile = GraphProfile.SIFT_CLASS,
        graph_ef: int = 0,
        graph_seed: int = 0,
        alpha: float | None = None,
        rules_enabled: bool = False,
        max_rounds: int | None = None,
        quoted_phrase: bool = False,
        identifier_token: bool = False,
        rarest_exact_document_frequency: int | None = None,
        cancel_token: CancelToken | None = None,
        deadline_ns: int = 0,
    ) -> QueryResult:
        handle = self._live_handle()
        if vector is None:
            vector_pointer, vector_len = s.FloatPointer(), 0
            probe = None
        else:
            probe = _vector(vector, dimensions=1)
            vector_pointer = probe.ctypes.data_as(s.FloatPointer)
            vector_len = probe.size
        if text is None:
            text_pointer, text_len, text_owner = s.UInt8Pointer(), 0, None
        elif isinstance(text, str):
            text_pointer, text_len, text_owner = _text_pointer(text)
        else:
            invalid_argument("text must be str or None")
        request = s.ZeQueryRequest(
            abi_size=ct.sizeof(s.ZeQueryRequest),
            abi_reserved=0,
            vector=vector_pointer,
            vector_len=vector_len,
            dimension=vector_len,
            text=text_pointer,
            text_len=text_len,
            k=k,
            thread_budget=thread_budget,
            has_tier=0 if tier is None else 1,
            tier=0 if tier is None else int(_enum_value(tier, Tier, "tier")),
            graph_profile=int(_enum_value(graph_profile, GraphProfile, "graph_profile")),
            reserved=0,
            graph_ef=graph_ef,
            graph_seed=graph_seed,
            has_alpha=0 if alpha is None else 1,
            rules_enabled=int(rules_enabled),
            alpha=0.0 if alpha is None else alpha,
            has_max_rounds=0 if max_rounds is None else 1,
            quoted_phrase=int(quoted_phrase),
            max_rounds=0 if max_rounds is None else max_rounds,
            identifier_token=int(identifier_token),
            has_rarest_exact_document_frequency=(
                0 if rarest_exact_document_frequency is None else 1
            ),
            rarest_exact_document_frequency=(
                0
                if rarest_exact_document_frequency is None
                else rarest_exact_document_frequency
            ),
            cancel_token=0 if cancel_token is None else cancel_token.value,
            deadline_ns=deadline_ns,
        )
        result = s.sized(s.ZeQueryResult)
        primary: BaseException | None = None
        try:
            status = self._call(LIBRARY.ze_query, handle, ct.byref(request), ct.byref(result))
            raise_for_status(status, handle)
            hits = tuple(
                QueryHit(
                    doc_id=_python_doc_id(hit.doc_id) if hit.has_document else None,
                    revision=int(hit.revision) if hit.has_revision else None,
                    score=float(hit.score),
                    vector_squared_l2=(
                        float(hit.vector_squared_l2) if hit.has_vector_score else None
                    ),
                    lexical_bm25=float(hit.lexical_bm25) if hit.has_lexical_score else None,
                )
                for hit in (result.hits[index] for index in range(result.hit_count))
            )
            fusion = (
                FusionReport(
                    FusionMethod(result.fusion_method),
                    float(result.effective_alpha),
                    int(result.fusion_rounds),
                )
                if result.has_fusion
                else None
            )
            return QueryResult(
                hits=hits,
                generation=int(result.generation),
                mode=QueryMode(result.mode),
                approximate=bool(result.approximate),
                exact_rescore=bool(result.exact_rescore),
                budget_exhausted=bool(result.budget_exhausted),
                fusion=fusion,
                embedding_epoch=int(result.embedding_epoch) if result.has_embedding_epoch else None,
                tokenizer_epoch=int(result.tokenizer_epoch) if result.has_tokenizer_epoch else None,
                dims_touched=int(result.dims_touched),
                bytes_read=int(result.bytes_read),
                docs_evaluated=int(result.docs_evaluated),
                postings_decoded=int(result.postings_decoded),
            )
        except BaseException as error:
            primary = error
            raise
        finally:
            del probe, text_owner
            free_status = self._call(LIBRARY.ze_query_result_free, ct.byref(result))
            if free_status != 0:
                if primary is None:
                    raise_for_status(free_status, handle)
                assert primary is not None
                primary.add_note(f"ze_query_result_free: {last_error_message(handle)}")

    def seal(self, cancel_token: CancelToken | None = None) -> GenerationReport:
        handle = self._live_handle()
        request = s.ZeSealRequest(
            abi_size=ct.sizeof(s.ZeSealRequest),
            abi_reserved=0,
            cancel_token=0 if cancel_token is None else cancel_token.value,
        )
        report = s.sized(s.ZeGenerationReport)
        raise_for_status(
            self._call(LIBRARY.ze_seal, handle, ct.byref(request), ct.byref(report)),
            handle,
        )
        return GenerationReport(int(report.generation))

    def drop_partition(self, start_ts: int, end_ts: int) -> PartitionReport:
        request = s.ZeDropPartitionRequest(
            abi_size=ct.sizeof(s.ZeDropPartitionRequest),
            abi_reserved=0,
            start_ts=start_ts,
            end_ts=end_ts,
        )
        return self._partition_call(LIBRARY.ze_drop_partition, request)

    def apply_retention(self, *, window: int, now_ts: int) -> PartitionReport:
        request = s.ZeRetentionRequest(
            abi_size=ct.sizeof(s.ZeRetentionRequest),
            abi_reserved=0,
            window=window,
            now_ts=now_ts,
        )
        return self._partition_call(LIBRARY.ze_apply_retention, request)

    def _partition_call(self, function: Any, request: ct.Structure) -> PartitionReport:
        handle = self._live_handle()
        report = s.sized(s.ZePartitionReport)
        raise_for_status(
            self._call(function, handle, ct.byref(request), ct.byref(report)),
            handle,
        )
        return PartitionReport(
            generation=int(report.generation),
            segments_dropped=int(report.segments_dropped),
            bytes_reclaimed=int(report.bytes_reclaimed),
            straddlers_skipped=int(report.straddlers_skipped),
            is_no_op=bool(report.is_no_op),
        )

    def purge(self, doc_ids: Sequence[int | tuple[int, int]]) -> PurgeTokenReport:
        handle = self._live_handle()
        ids = (s.ZeDocId * len(doc_ids))(*[_doc_id(value) for value in doc_ids])
        request = s.ZePurgeRequest(
            abi_size=ct.sizeof(s.ZePurgeRequest),
            abi_reserved=0,
            doc_ids=ct.cast(ids, ct.POINTER(s.ZeDocId)),
            doc_id_count=len(ids),
        )
        report = s.sized(s.ZePurgeTokenReport)
        raise_for_status(
            self._call(LIBRARY.ze_purge, handle, ct.byref(request), ct.byref(report)),
            handle,
        )
        return PurgeTokenReport(
            token_id=int(report.token_id),
            generation=int(report.generation),
            unknown_id_count=int(report.unknown_id_count),
            is_no_op=bool(report.is_no_op),
        )

    def await_physical_purge(self, token_id: int) -> PurgeReport:
        handle = self._live_handle()
        request = s.ZeAwaitPurgeRequest(
            abi_size=ct.sizeof(s.ZeAwaitPurgeRequest),
            abi_reserved=0,
            token_id=token_id,
        )
        report = s.sized(s.ZePurgeReport)
        raise_for_status(
            self._call(
                LIBRARY.ze_await_physical_purge,
                handle,
                ct.byref(request),
                ct.byref(report),
            ),
            handle,
        )
        return PurgeReport(
            generation=int(report.generation),
            segments_rewritten=int(report.segments_rewritten),
            unknown_id_count=int(report.unknown_id_count),
            wal_rewritten=bool(report.wal_rewritten),
            is_no_op=bool(report.is_no_op),
        )

    def maintain(self, *, wall_time_ns: int, bytes: int) -> MaintainReport:
        handle = self._live_handle()
        request = s.ZeMaintainRequest(
            abi_size=ct.sizeof(s.ZeMaintainRequest),
            abi_reserved=0,
            wall_time_ns=wall_time_ns,
            bytes=bytes,
        )
        report = s.sized(s.ZeMaintainReport)
        raise_for_status(
            self._call(LIBRARY.ze_maintain, handle, ct.byref(request), ct.byref(report)),
            handle,
        )
        return MaintainReport(
            graphs_built=int(report.graphs_built),
            bytes_consumed=int(report.bytes_consumed),
            checkpoints_resumed=int(report.checkpoints_resumed),
            status=MaintenanceStatus(report.status),
        )

    def epoch_current(self) -> EpochIdentity:
        handle = self._live_handle()
        report = s.sized(s.ZeEpochIdentity)
        raise_for_status(
            self._call(LIBRARY.ze_epoch_current, handle, ct.byref(report)),
            handle,
        )
        return EpochIdentity(int(report.embedding_epoch), int(report.tokenizer_epoch))

    def epoch_switch_alias(self, epoch: EmbeddingEpoch) -> EpochAliasReport:
        handle = self._live_handle()
        request, owners = _epoch_request(epoch)
        report = s.sized(s.ZeEpochAliasReport)
        status = self._call(
            LIBRARY.ze_epoch_switch_alias,
            handle,
            ct.byref(request),
            ct.byref(report),
        )
        del owners
        raise_for_status(status, handle)
        return EpochAliasReport(
            generation=int(report.generation),
            previous_embedding_epoch=int(report.previous_embedding_epoch),
            previous_tokenizer_epoch=int(report.previous_tokenizer_epoch),
            published_embedding_epoch=int(report.published_embedding_epoch),
            published_tokenizer_epoch=int(report.published_tokenizer_epoch),
            manifest_committed=bool(report.manifest_committed),
        )

    def epoch_drop(self, epoch: EmbeddingEpoch) -> EpochDropReport:
        handle = self._live_handle()
        request, owners = _epoch_request(epoch)
        report = s.sized(s.ZeEpochDropReport)
        status = self._call(
            LIBRARY.ze_epoch_drop,
            handle,
            ct.byref(request),
            ct.byref(report),
        )
        del owners
        raise_for_status(status, handle)
        return EpochDropReport(
            generation=int(report.generation),
            segments_dropped=int(report.segments_dropped),
            bytes_reclaimed=int(report.bytes_reclaimed),
        )


def open(
    path: str | Path,
    *,
    access_mode: AccessMode | str = AccessMode.READ_WRITE,
    durability: DurabilityMode | str = DurabilityMode.DERIVED,
    commit_tier: CommitTier | str = CommitTier.ORDERED,
    reader_drain_timeout_ms: int = 250,
    max_resident_bytes: int = (1 << 64) - 1,
    max_temp_bytes: int = (1 << 64) - 1,
    epoch: EmbeddingEpoch | None = None,
) -> Store:
    """Open one store directory, optionally declaring its complete epoch."""

    request, path_owner = _open_request(
        path,
        access_mode,
        durability,
        commit_tier,
        reader_drain_timeout_ms,
        max_resident_bytes,
        max_temp_bytes,
    )
    handle = ct.c_uint64()
    if epoch is None:
        status = LIBRARY.ze_open(ct.byref(request), ct.byref(handle))
    else:
        epoch_request, epoch_owners = _epoch_request(epoch)
        status = LIBRARY.ze_open_with_epoch(
            ct.byref(request),
            ct.byref(epoch_request),
            ct.byref(handle),
        )
        del epoch_owners
    del path_owner
    raise_for_status(status, int(handle.value))
    return Store(int(handle.value))


def open_with_epoch(
    path: str | Path,
    epoch: EmbeddingEpoch,
    *,
    access_mode: AccessMode | str = AccessMode.READ_WRITE,
    durability: DurabilityMode | str = DurabilityMode.DERIVED,
    commit_tier: CommitTier | str = CommitTier.ORDERED,
    reader_drain_timeout_ms: int = 250,
    max_resident_bytes: int = (1 << 64) - 1,
    max_temp_bytes: int = (1 << 64) - 1,
) -> Store:
    """Open one store while explicitly declaring its complete epoch."""

    return open(
        path,
        access_mode=access_mode,
        durability=durability,
        commit_tier=commit_tier,
        reader_drain_timeout_ms=reader_drain_timeout_ms,
        max_resident_bytes=max_resident_bytes,
        max_temp_bytes=max_temp_bytes,
        epoch=epoch,
    )
