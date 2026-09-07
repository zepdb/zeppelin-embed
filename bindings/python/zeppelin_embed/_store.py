"""Pythonic Store wrapper over the frozen Zeppelin Embed C ABI."""

from __future__ import annotations

import ctypes as ct
import os
from collections.abc import Iterator, Sequence
from enum import IntEnum
from pathlib import Path
from typing import Any, Self, TypeVar

import numpy as np

from . import _structures as s
from ._errors import ERROR_TYPES, invalid_argument, last_error_message, raise_for_status
from ._library import LIBRARY
from ._types import (
    AccessMode,
    AttributeDefinition,
    AttributeType,
    AttributeValue,
    CommitTier,
    ComputeUnits,
    CountResult,
    DurabilityMode,
    EmbeddingEpoch,
    EmbeddingRuntime,
    EpochAliasReport,
    EpochDropReport,
    EpochIdentity,
    FusionMethod,
    FusionReport,
    GenerationReport,
    GetResult,
    GraphProfile,
    MaintainReport,
    MaintenanceStatus,
    MutationReport,
    NamespaceSpec,
    Normalization,
    PartitionReport,
    PurgeReport,
    PurgeTokenReport,
    QueryHit,
    QueryMode,
    QueryResult,
    ScanCursor,
    ScanPage,
    SearchHit,
    SearchResult,
    StateReport,
    StatsReport,
    StoredDocument,
    StoreState,
    TextHit,
    TextLegs,
    TextQueryResult,
    Tier,
    VectorSpace,
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


def _attribute_value(
    attribute: AttributeValue,
    owners: list[object],
    *,
    allow_null: bool = True,
) -> s.ZeAttributeValue:
    attribute_type = _enum_value(
        attribute.attribute_type, AttributeType, "attribute_type"
    )
    value = attribute.value
    result = s.ZeAttributeValue(attribute_id=attribute.attribute_id)
    if value is None:
        if not allow_null:
            invalid_argument("filter values cannot be null")
        result.value_type = 0
    elif attribute_type is AttributeType.U64:
        if isinstance(value, bool) or not isinstance(value, int) or not 0 <= value < 1 << 64:
            invalid_argument("U64 attribute value must fit an unsigned 64-bit integer")
        result.value_type = 1
        result.u64_value = value
    elif attribute_type is AttributeType.I64:
        if (
            isinstance(value, bool)
            or not isinstance(value, int)
            or not -(1 << 63) <= value < 1 << 63
        ):
            invalid_argument("I64 attribute value must fit a signed 64-bit integer")
        result.value_type = 2
        result.i64_value = value
    elif attribute_type is AttributeType.F64:
        if isinstance(value, bool) or not isinstance(value, (int, float)):
            invalid_argument("F64 attribute value must be numeric")
        result.value_type = 3
        result.f64_value = float(value)
    elif attribute_type is AttributeType.BOOL:
        if not isinstance(value, bool):
            invalid_argument("Bool attribute value must be bool")
        result.value_type = 4
        result.bool_value = int(value)
    else:
        if not isinstance(value, str):
            invalid_argument("string attribute value must be str")
        pointer, length, owner = _text_pointer(value)
        if owner is not None:
            owners.append(owner)
        result.value_type = 5
        result.string_value = pointer
        result.string_len = length
    return result


def _filter_field(
    field: str | int,
    attribute_ids: dict[str, int],
    attribute_types: dict[int, AttributeType],
    value: object | None,
) -> tuple[int, AttributeType]:
    if isinstance(field, str):
        if field == "ts":
            attribute_id = 0
        else:
            try:
                attribute_id = attribute_ids[field]
            except KeyError:
                invalid_argument(f"unknown filter field {field!r}")
    elif isinstance(field, int) and not isinstance(field, bool) and 0 <= field < 1 << 32:
        attribute_id = field
    else:
        invalid_argument("filter field must be an attribute name or u32 id")
    attribute_type = attribute_types.get(attribute_id)
    if attribute_type is None:
        if attribute_id == 0 or (isinstance(value, int) and value < 0):
            attribute_type = AttributeType.I64
        elif isinstance(value, bool):
            attribute_type = AttributeType.BOOL
        elif isinstance(value, int):
            attribute_type = AttributeType.U64
        elif isinstance(value, float):
            attribute_type = AttributeType.F64
        elif isinstance(value, str):
            attribute_type = AttributeType.RAW_STRING
        else:
            invalid_argument(f"cannot infer the type of filter field {field!r}")
    return attribute_id, attribute_type


def _flatten_filter(
    value: dict[str, Any],
    attribute_ids: dict[str, int],
    attribute_types: dict[int, AttributeType],
) -> tuple[s.ZeFilter, list[object]]:
    if not isinstance(value, dict):
        invalid_argument("filter must be built with Filter")
    slots: list[s.ZeFilterNode | None] = [None]
    owners: list[object] = []
    active: set[int] = set()
    operations = {
        "eq": 1,
        "not_eq": 2,
        "in": 3,
        "not_in": 4,
        "range": 5,
        "exists": 6,
        "is_null": 7,
        "and": 8,
        "or": 9,
        "not": 10,
    }

    def fill(node: dict[str, Any], index: int, depth: int) -> None:
        if not isinstance(node, dict):
            invalid_argument("filter children must be built with Filter")
        if depth > 32:
            invalid_argument("filter depth exceeds 32")
        identity = id(node)
        if identity in active:
            invalid_argument("filter tree contains a cycle")
        active.add(identity)
        try:
            operation = node.get("op")
            if not isinstance(operation, str) or operation not in operations:
                invalid_argument(f"filter operator {operation!r} is not supported")
            op = operations[operation]
            if op >= 8:
                if op == 10:
                    if "filter" not in node:
                        invalid_argument("filter not requires exactly one child")
                    children: list[Any] = [node["filter"]]
                else:
                    raw_children = node.get("filters")
                    if not isinstance(raw_children, list):
                        invalid_argument(f"filter {operation} requires a filter list")
                    children = raw_children
                children_start = len(slots)
                slots.extend([None] * len(children))
                slots[index] = s.ZeFilterNode(
                    op=op,
                    children_start=children_start,
                    children_count=len(children),
                )
                for offset, child in enumerate(children):
                    fill(child, children_start + offset, depth + 1)
                return
            field = node.get("field")
            if field is None:
                invalid_argument(f"filter {operation} requires a field")
            if op in (1, 2):
                if "value" not in node:
                    invalid_argument(f"filter {operation} requires one value")
                node_values: tuple[Any, ...] = (node["value"],)
            elif op in (3, 4):
                raw_values = node.get("values")
                if not isinstance(raw_values, (list, tuple)):
                    invalid_argument(f"filter {operation} requires a value list")
                node_values = tuple(raw_values)
            else:
                node_values = ()
            if op == 5:
                if "gte" in node and "gt" in node:
                    invalid_argument("filter range cannot contain both gte and gt")
                if "lte" in node and "lt" in node:
                    invalid_argument("filter range cannot contain both lte and lt")
                lower = node.get("gte", node.get("gt"))
                upper = node.get("lte", node.get("lt"))
                if lower is None and upper is None:
                    invalid_argument("filter range requires at least one bound")
                lower_inclusive = "gte" in node
                upper_inclusive = "lte" in node
            else:
                lower = upper = None
                lower_inclusive = upper_inclusive = False
            sample = next((item for item in node_values if item is not None), None)
            if sample is None:
                sample = lower if lower is not None else upper
            attribute_id, attribute_type = _filter_field(
                field, attribute_ids, attribute_types, sample
            )
            values = (s.ZeAttributeValue * len(node_values))(
                *[
                    _attribute_value(
                        AttributeValue(attribute_id, attribute_type, item),
                        owners,
                        allow_null=False,
                    )
                    for item in node_values
                ]
            )
            values_pointer = (
                ct.cast(values, ct.POINTER(s.ZeAttributeValue))
                if node_values
                else ct.POINTER(s.ZeAttributeValue)()
            )
            if node_values:
                owners.append(values)
            lower_value = s.ZeAttributeValue()
            if lower is not None:
                lower_value = _attribute_value(
                    AttributeValue(attribute_id, attribute_type, lower),
                    owners,
                    allow_null=False,
                )
            upper_value = s.ZeAttributeValue()
            if upper is not None:
                upper_value = _attribute_value(
                    AttributeValue(attribute_id, attribute_type, upper),
                    owners,
                    allow_null=False,
                )
            slots[index] = s.ZeFilterNode(
                op=op,
                attribute_id=attribute_id,
                values=values_pointer,
                value_count=len(node_values),
                has_lower=int(lower is not None),
                lower=lower_value,
                lower_inclusive=int(lower_inclusive),
                has_upper=int(upper is not None),
                upper=upper_value,
                upper_inclusive=int(upper_inclusive),
            )
        finally:
            active.remove(identity)

    fill(value, 0, 1)
    filled: list[s.ZeFilterNode] = []
    for slot in slots:
        if slot is None:
            invalid_argument("filter tree contains an unfilled child index")
        filled.append(slot)
    nodes = (s.ZeFilterNode * len(filled))(*filled)
    owners.append(nodes)
    return (
        s.ZeFilter(
            abi_size=ct.sizeof(s.ZeFilter),
            abi_reserved=0,
            nodes=ct.cast(nodes, ct.POINTER(s.ZeFilterNode)),
            node_count=len(nodes),
            root=0,
        ),
        owners,
    )


def _returned_attribute(
    value: s.ZeAttributeValue,
    attribute_types: dict[int, AttributeType],
) -> AttributeValue:
    attribute_id = int(value.attribute_id)
    if value.value_type == 0:
        attribute_type = attribute_types.get(attribute_id, AttributeType.RAW_STRING)
        payload: int | float | bool | str | None = None
    elif value.value_type == 1:
        attribute_type = AttributeType.U64
        payload = int(value.u64_value)
    elif value.value_type == 2:
        attribute_type = AttributeType.I64
        payload = int(value.i64_value)
    elif value.value_type == 3:
        attribute_type = AttributeType.F64
        payload = float(value.f64_value)
    elif value.value_type == 4:
        attribute_type = AttributeType.BOOL
        payload = bool(value.bool_value)
    elif value.value_type == 5:
        attribute_type = attribute_types.get(attribute_id, AttributeType.RAW_STRING)
        payload = ct.string_at(value.string_value, value.string_len).decode("utf-8")
    else:
        invalid_argument("returned attribute value type is invalid")
    return AttributeValue(attribute_id, attribute_type, payload)


def _stored_document(
    value: s.ZeStoredDocument,
    attribute_types: dict[int, AttributeType],
    *,
    include_vector: bool,
    include_text: bool,
    include_metadata: bool,
    include_attributes: bool,
) -> StoredDocument | None:
    if not value.has_document:
        return None
    vector = None
    if include_vector and value.vector and value.vector_len:
        vector = np.ctypeslib.as_array(value.vector, shape=(value.vector_len,)).copy()
    text = None
    if include_text:
        text = ct.string_at(value.text, value.text_len).decode("utf-8")
    metadata = None
    if include_metadata:
        metadata = ct.string_at(value.metadata, value.metadata_len)
    attributes = None
    if include_attributes:
        attributes = tuple(
            _returned_attribute(value.attributes[index], attribute_types)
            for index in range(value.attribute_count)
        )
    return StoredDocument(
        doc_id=_python_doc_id(value.doc_id),
        revision=int(value.revision),
        timestamp=int(value.timestamp),
        vector=vector,
        text=text,
        metadata=metadata,
        attributes=attributes,
    )


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

    def __init__(
        self,
        handle: int,
        *,
        text: bool = False,
        attribute_ids: dict[str, int] | None = None,
        attribute_types: dict[int, AttributeType] | None = None,
        vector_dimensions: int | None = None,
        record_only: bool = False,
    ) -> None:
        self._handle = handle
        self._abi_call_count = 0
        self._text = text
        self._attribute_ids = {} if attribute_ids is None else dict(attribute_ids)
        self._attribute_types = {} if attribute_types is None else dict(attribute_types)
        self._vector_dimensions = vector_dimensions
        self._record_only = record_only

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

    def upsert(self, documents: Sequence[StoredDocument]) -> MutationReport:
        """Atomically upsert documents carrying typed schema attributes."""

        handle = self._live_handle()
        if self._record_only:
            dimension = 0
        elif self._vector_dimensions is not None:
            dimension = self._vector_dimensions
        else:
            first_vector = next(
                (document.vector for document in documents if document.vector is not None), None
            )
            if first_vector is None:
                invalid_argument("vector namespace upsert requires document vectors")
            dimension = int(_vector(first_vector, dimensions=1).size)
        records = (s.ZeUpsertDocument * len(documents))()
        owners: list[object] = [records]
        for index, document in enumerate(documents):
            if not isinstance(document, StoredDocument):
                invalid_argument("documents must contain StoredDocument values")
            if self._record_only:
                if document.vector is not None:
                    invalid_argument("record-only namespace does not accept document vectors")
                vector_pointer, vector_len = s.FloatPointer(), 0
            else:
                if document.vector is None:
                    invalid_argument("vector namespace upsert requires document vectors")
                vector = _vector(document.vector, dimensions=1)
                if vector.size != dimension:
                    invalid_argument(
                        f"document vector length {vector.size} does not match {dimension}"
                    )
                owners.append(vector)
                vector_pointer = vector.ctypes.data_as(s.FloatPointer)
                vector_len = int(vector.size)
            if document.text is None:
                text_pointer, text_len = s.UInt8Pointer(), 0
            elif isinstance(document.text, str):
                text_pointer, text_len, owner = _text_pointer(document.text)
                if owner is not None:
                    owners.append(owner)
            else:
                invalid_argument("document text must be str or None")
            if document.metadata is None:
                metadata_pointer, metadata_len = s.UInt8Pointer(), 0
            elif isinstance(document.metadata, bytes):
                metadata_pointer, owner = _bytes_pointer(document.metadata)
                metadata_len = len(document.metadata)
                if owner is not None:
                    owners.append(owner)
            else:
                invalid_argument("document metadata must be bytes or None")
            attributes = () if document.attributes is None else document.attributes
            attribute_array = (s.ZeAttributeValue * len(attributes))(
                *[_attribute_value(attribute, owners) for attribute in attributes]
            )
            if attributes:
                owners.append(attribute_array)
                attribute_pointer = ct.cast(
                    attribute_array, ct.POINTER(s.ZeAttributeValue)
                )
            else:
                attribute_pointer = ct.POINTER(s.ZeAttributeValue)()
            records[index] = s.ZeUpsertDocument(
                abi_size=ct.sizeof(s.ZeUpsertDocument),
                abi_reserved=0,
                document=s.ZeIngestDocument(
                    abi_size=ct.sizeof(s.ZeIngestDocument),
                    abi_reserved=0,
                    doc_id=_doc_id(document.doc_id),
                    revision=document.revision,
                    timestamp=document.timestamp,
                    vector=vector_pointer,
                    vector_len=vector_len,
                    metadata=metadata_pointer,
                    metadata_len=metadata_len,
                    text=text_pointer,
                    text_len=text_len,
                ),
                attributes=attribute_pointer,
                attribute_count=len(attributes),
            )
        request = s.ZeUpsertRequest(
            abi_size=ct.sizeof(s.ZeUpsertRequest),
            abi_reserved=0,
            documents=ct.cast(records, ct.POINTER(s.ZeUpsertDocument)),
            document_count=len(records),
            dimension=dimension,
        )
        report = s.sized(s.ZeMutationReport)
        status = self._call(LIBRARY.ze_upsert, handle, ct.byref(request), ct.byref(report))
        del owners
        raise_for_status(status, handle)
        return MutationReport(int(report.sequence), int(report.generation))

    def get(
        self,
        ids: Sequence[int | tuple[int, int]],
        *,
        vector: bool = True,
        text: bool = True,
        metadata: bool = True,
        attributes: bool = True,
    ) -> GetResult:
        """Read requested ids in caller order from one pinned generation."""

        handle = self._live_handle()
        doc_ids = (s.ZeDocId * len(ids))(*[_doc_id(value) for value in ids])
        request = s.ZeGetRequest(
            abi_size=ct.sizeof(s.ZeGetRequest),
            abi_reserved=0,
            ids=ct.cast(doc_ids, ct.POINTER(s.ZeDocId)),
            id_count=len(doc_ids),
            include_vector=int(vector),
            include_text=int(text),
            include_metadata=int(metadata),
            include_attributes=int(attributes),
        )
        result = s.sized(s.ZeGetResult)
        primary: BaseException | None = None
        try:
            status = self._call(LIBRARY.ze_get, handle, ct.byref(request), ct.byref(result))
            raise_for_status(status, handle)
            returned = tuple(
                _stored_document(
                    result.documents[index],
                    self._attribute_types,
                    include_vector=vector and not self._record_only,
                    include_text=text,
                    include_metadata=metadata,
                    include_attributes=attributes,
                )
                for index in range(result.document_count)
            )
            return GetResult(returned, int(result.missing_count), int(result.generation))
        except BaseException as error:
            primary = error
            raise
        finally:
            del doc_ids
            free_status = self._call(LIBRARY.ze_get_result_free, ct.byref(result))
            if free_status != 0:
                if primary is None:
                    raise_for_status(free_status, handle)
                assert primary is not None
                primary.add_note(f"ze_get_result_free: {last_error_message(handle)}")

    def scan(
        self,
        *,
        cursor: ScanCursor | None = None,
        limit: int = 1_024,
        order: str = "storage",
        vector: bool = True,
        text: bool = True,
        metadata: bool = True,
        attributes: bool = True,
        timestamp_range: tuple[int, int] | None = None,
        filter: dict[str, Any] | None = None,
        cancel_token: CancelToken | None = None,
        deadline_ns: int = 0,
    ) -> ScanPage:
        """Enumerate one ordered and optionally filtered document page."""

        handle = self._live_handle()
        orders = {
            "storage": 0,
            "timestamp_ascending": 1,
            "timestamp-ascending": 1,
            "timestamp_descending": 2,
            "timestamp-descending": 2,
        }
        try:
            order_value = orders[order.lower()]
        except (AttributeError, KeyError):
            invalid_argument(f"scan order {order!r} is not supported")
        if cursor is None:
            cursor_generation = cursor_next_row = cursor_phase = 0
            cursor_segment_id = (ct.c_uint8 * 16)()
        elif isinstance(cursor, ScanCursor):
            cursor_generation = cursor._generation
            cursor_next_row = cursor._next_row
            cursor_phase = cursor._phase
            cursor_segment_id = (ct.c_uint8 * 16).from_buffer_copy(cursor._segment_id)
        else:
            invalid_argument("cursor must be a ScanCursor or None")
        if timestamp_range is None:
            has_timestamp_range, start_ts, end_ts = 0, 0, 0
        elif isinstance(timestamp_range, tuple) and len(timestamp_range) == 2:
            has_timestamp_range = 1
            start_ts, end_ts = timestamp_range
        else:
            invalid_argument("timestamp_range must be a (start, end) tuple or None")
        if filter is None:
            filter_pointer = ct.POINTER(s.ZeFilter)()
            filter_owners: list[object] = []
        else:
            flat_filter, filter_owners = _flatten_filter(
                filter, self._attribute_ids, self._attribute_types
            )
            filter_pointer = ct.pointer(flat_filter)
            filter_owners.extend((flat_filter, filter_pointer))
        request = s.ZeScanRequest(
            abi_size=ct.sizeof(s.ZeScanRequest),
            abi_reserved=0,
            cursor_generation=cursor_generation,
            cursor_segment_id=cursor_segment_id,
            cursor_next_row=cursor_next_row,
            cursor_phase=cursor_phase,
            limit=limit,
            order=order_value,
            include_vector=int(vector),
            include_text=int(text),
            include_metadata=int(metadata),
            include_attributes=int(attributes),
            has_timestamp_range=has_timestamp_range,
            start_ts=start_ts,
            end_ts=end_ts,
            filter=filter_pointer,
            cancel_token=0 if cancel_token is None else cancel_token.value,
            deadline_ns=deadline_ns,
        )
        result = s.sized(s.ZeScanResult)
        primary: BaseException | None = None
        try:
            status = self._call(LIBRARY.ze_scan, handle, ct.byref(request), ct.byref(result))
            raise_for_status(status, handle)
            returned: list[StoredDocument] = []
            for index in range(result.document_count):
                document = _stored_document(
                    result.documents[index],
                    self._attribute_types,
                    include_vector=vector and not self._record_only,
                    include_text=text,
                    include_metadata=metadata,
                    include_attributes=attributes,
                )
                if document is None:
                    raise ERROR_TYPES[22]("scan returned a missing document")
                returned.append(document)
            next_cursor = (
                ScanCursor(
                    int(result.generation),
                    bytes(result.next_segment_id),
                    int(result.next_row),
                    int(result.next_phase),
                )
                if result.has_more
                else None
            )
            return ScanPage(tuple(returned), next_cursor, int(result.generation))
        except BaseException as error:
            primary = error
            raise
        finally:
            del filter_owners
            free_status = self._call(LIBRARY.ze_scan_result_free, ct.byref(result))
            if free_status != 0:
                if primary is None:
                    raise_for_status(free_status, handle)
                assert primary is not None
                primary.add_note(f"ze_scan_result_free: {last_error_message(handle)}")

    def iter_documents(
        self,
        *,
        order: str = "storage",
        vector: bool = True,
        text: bool = True,
        metadata: bool = True,
        attributes: bool = True,
        filter: dict[str, Any] | None = None,
    ) -> Iterator[StoredDocument]:
        """Follow scan cursors until every matching live document is yielded."""

        cursor = None
        while True:
            page = self.scan(
                cursor=cursor,
                order=order,
                vector=vector,
                text=text,
                metadata=metadata,
                attributes=attributes,
                filter=filter,
            )
            yield from page.documents
            cursor = page.cursor
            if cursor is None:
                return

    def count(
        self,
        *,
        filter: dict[str, Any] | None = None,
        timestamp_range: tuple[int, int] | None = None,
    ) -> CountResult:
        """Count live documents matching a filter and timestamp range."""

        handle = self._live_handle()
        if timestamp_range is None:
            has_timestamp_range, start_ts, end_ts = 0, 0, 0
        elif isinstance(timestamp_range, tuple) and len(timestamp_range) == 2:
            has_timestamp_range = 1
            start_ts, end_ts = timestamp_range
        else:
            invalid_argument("timestamp_range must be a (start, end) tuple or None")
        if filter is None:
            filter_pointer = ct.POINTER(s.ZeFilter)()
            filter_owners: list[object] = []
        else:
            flat_filter, filter_owners = _flatten_filter(
                filter, self._attribute_ids, self._attribute_types
            )
            filter_pointer = ct.pointer(flat_filter)
            filter_owners.extend((flat_filter, filter_pointer))
        request = s.ZeCountRequest(
            abi_size=ct.sizeof(s.ZeCountRequest),
            abi_reserved=0,
            filter=filter_pointer,
            has_timestamp_range=has_timestamp_range,
            start_ts=start_ts,
            end_ts=end_ts,
        )
        result = s.sized(s.ZeCountResult)
        status = self._call(LIBRARY.ze_count, handle, ct.byref(request), ct.byref(result))
        del filter_owners
        raise_for_status(status, handle)
        return CountResult(int(result.count), int(result.generation))

    def ingest_text(
        self,
        ids: Sequence[int | tuple[int, int]],
        texts: Sequence[str],
        *,
        revisions: Sequence[int] | None = None,
        embed_batch_size: int = 32,
        seal_every: int = 512,
        channel_capacity: int = 2,
    ) -> MutationReport:
        """Embed and ingest caller text through the bounded native pipeline."""
        if not self._text:
            invalid_argument("ingest_text requires a store opened with open_text")
        _same_length("texts", texts, len(ids))
        revisions = [1] * len(ids) if revisions is None else revisions
        _same_length("revisions", revisions, len(ids))
        documents = (s.ZeTextDocument * len(ids))()
        owners: list[object] = [documents]
        for index, value in enumerate(texts):
            pointer, length, owner = _text_pointer(value)
            if owner is not None:
                owners.append(owner)
            documents[index] = s.ZeTextDocument(
                abi_size=ct.sizeof(s.ZeTextDocument),
                abi_reserved=0,
                doc_id=_doc_id(ids[index]),
                revision=revisions[index],
                text=pointer,
                text_len=length,
            )
        request = s.ZeTextIngestRequest(
            abi_size=ct.sizeof(s.ZeTextIngestRequest),
            abi_reserved=0,
            documents=ct.cast(documents, ct.POINTER(s.ZeTextDocument)),
            document_count=len(documents),
            embed_batch_size=embed_batch_size,
            seal_every=seal_every,
            channel_capacity=channel_capacity,
        )
        report = s.sized(s.ZeMutationReport)
        handle = self._live_handle()
        status = self._call(
            LIBRARY.ze_text_ingest, handle, ct.byref(request), ct.byref(report)
        )
        del owners
        raise_for_status(status, handle)
        return MutationReport(int(report.sequence), int(report.generation))

    def query_text(
        self,
        text: str,
        *,
        k: int = 10,
        legs: TextLegs | str = TextLegs.HYBRID,
    ) -> TextQueryResult:
        """Embed one raw query and return stored text with every hit."""
        if not self._text:
            invalid_argument("query_text requires a store opened with open_text")
        pointer, length, owner = _text_pointer(text)
        request = s.ZeTextQueryRequest(
            abi_size=ct.sizeof(s.ZeTextQueryRequest),
            abi_reserved=0,
            text=pointer,
            text_len=length,
            k=k,
            legs=int(_enum_value(legs, TextLegs, "legs")),
            reserved=0,
        )
        result = s.sized(s.ZeTextQueryResult)
        handle = self._live_handle()
        primary: BaseException | None = None
        try:
            status = self._call(
                LIBRARY.ze_text_query, handle, ct.byref(request), ct.byref(result)
            )
            raise_for_status(status, handle)
            hits = tuple(
                TextHit(
                    doc_id=_python_doc_id(hit.doc_id),
                    revision=int(hit.revision),
                    chunk=int(hit.chunk),
                    text=ct.string_at(hit.text, hit.text_len).decode("utf-8"),
                    score=float(hit.score),
                    vector_squared_l2=(
                        float(hit.vector_squared_l2) if hit.has_vector_score else None
                    ),
                    lexical_bm25=(
                        float(hit.lexical_bm25) if hit.has_lexical_score else None
                    ),
                )
                for hit in (result.hits[index] for index in range(result.hit_count))
            )
            return TextQueryResult(
                hits=hits,
                embedding_epoch=int(result.embedding_epoch),
                tokenizer_epoch=int(result.tokenizer_epoch),
            )
        except BaseException as error:
            primary = error
            raise
        finally:
            del owner
            free_status = self._call(LIBRARY.ze_text_query_result_free, ct.byref(result))
            if free_status != 0:
                if primary is None:
                    raise_for_status(free_status, handle)
                assert primary is not None
                primary.add_note(
                    f"ze_text_query_result_free: {last_error_message(handle)}"
                )

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
        filter: dict[str, Any] | None = None,
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
        if filter is None:
            function = LIBRARY.ze_search
            call_request: ct.Structure = request
            filter_owners: list[object] = []
        else:
            flat_filter, filter_owners = _flatten_filter(
                filter, self._attribute_ids, self._attribute_types
            )
            filter_pointer = ct.pointer(flat_filter)
            filter_owners.extend((flat_filter, filter_pointer))
            call_request = s.ZeSearchFilteredRequest(
                abi_size=ct.sizeof(s.ZeSearchFilteredRequest),
                abi_reserved=0,
                search=request,
                filter=filter_pointer,
            )
            function = LIBRARY.ze_search_filtered
        primary: BaseException | None = None
        try:
            status = self._call(function, handle, ct.byref(call_request), ct.byref(result))
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
            del filter_owners
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
        last_as_prefix: bool = False,
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
            lexical_flags=int(last_as_prefix),
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


def open_namespace(
    root: str | Path,
    name: str,
    spec: NamespaceSpec,
    *,
    access_mode: AccessMode | str = AccessMode.READ_WRITE,
    durability: DurabilityMode | str = DurabilityMode.DERIVED,
    commit_tier: CommitTier | str = CommitTier.ORDERED,
    reader_drain_timeout_ms: int = 250,
    max_resident_bytes: int = (1 << 64) - 1,
    max_temp_bytes: int = (1 << 64) - 1,
) -> Store:
    """Open or idempotently create one namespace under a database root."""

    if not isinstance(spec, NamespaceSpec):
        invalid_argument("spec must be a NamespaceSpec")
    open_request, open_owner = _open_request(
        root,
        access_mode,
        durability,
        commit_tier,
        reader_drain_timeout_ms,
        max_resident_bytes,
        max_temp_bytes,
    )
    root_bytes = os.fspath(root).encode("utf-8", errors="strict")
    root_pointer, root_owner = _bytes_pointer(root_bytes)
    name_pointer, name_len, name_owner = _text_pointer(name)
    owners: list[object] = [open_owner]
    if root_owner is not None:
        owners.append(root_owner)
    if name_owner is not None:
        owners.append(name_owner)
    definitions = (s.ZeAttributeDefinition * len(spec.attributes))()
    attribute_ids: dict[str, int] = {}
    attribute_types: dict[int, AttributeType] = {0: AttributeType.I64}
    for index, definition in enumerate(spec.attributes):
        if not isinstance(definition, AttributeDefinition):
            invalid_argument("namespace attributes must be AttributeDefinition values")
        pointer, length, owner = _text_pointer(definition.name)
        if owner is not None:
            owners.append(owner)
        attribute_type = _enum_value(
            definition.attribute_type, AttributeType, "attribute_type"
        )
        definitions[index] = s.ZeAttributeDefinition(
            attribute_id=definition.attribute_id,
            name=pointer,
            name_len=length,
            attribute_type=int(attribute_type),
            nullable=int(definition.nullable),
        )
        attribute_ids[definition.name] = definition.attribute_id
        attribute_types[definition.attribute_id] = attribute_type
    owners.append(definitions)
    vector_space = spec.vector_space
    if vector_space is None:
        has_vector_space = 0
        dimensions = 0
        normalization = Normalization.NONE
        epoch_pointer = ct.POINTER(s.ZeEpochRequest)()
        vector_dimensions = None
        record_only = True
    elif isinstance(vector_space, VectorSpace):
        has_vector_space = 1
        dimensions = vector_space.dimensions
        normalization = _enum_value(
            vector_space.normalization, Normalization, "normalization"
        )
        vector_dimensions = vector_space.dimensions
        record_only = False
        if vector_space.epoch is None:
            epoch_pointer = ct.POINTER(s.ZeEpochRequest)()
        else:
            epoch_request, epoch_owners = _epoch_request(vector_space.epoch)
            epoch_pointer = ct.pointer(epoch_request)
            owners.extend((epoch_request, epoch_pointer, *epoch_owners))
    else:
        invalid_argument("vector_space must be a VectorSpace or None")
    namespace_spec = s.ZeNamespaceSpec(
        abi_size=ct.sizeof(s.ZeNamespaceSpec),
        abi_reserved=0,
        attributes=ct.cast(definitions, ct.POINTER(s.ZeAttributeDefinition)),
        attribute_count=len(definitions),
        has_vector_space=has_vector_space,
        dimensions=dimensions,
        normalization=int(normalization),
        epoch=epoch_pointer,
    )
    namespace_spec_pointer = ct.pointer(namespace_spec)
    owners.extend((namespace_spec, namespace_spec_pointer))
    request = s.ZeNamespaceOpenRequest(
        abi_size=ct.sizeof(s.ZeNamespaceOpenRequest),
        abi_reserved=0,
        root=root_pointer,
        root_len=len(root_bytes),
        name=name_pointer,
        name_len=name_len,
        open=open_request,
        spec=namespace_spec_pointer,
    )
    handle = ct.c_uint64()
    status = LIBRARY.ze_namespace_open(ct.byref(request), ct.byref(handle))
    del owners
    raise_for_status(status, int(handle.value))
    return Store(
        int(handle.value),
        attribute_ids=attribute_ids,
        attribute_types=attribute_types,
        vector_dimensions=vector_dimensions,
        record_only=record_only,
    )


def list_namespaces(root: str | Path) -> list[str]:
    """Return namespace names immediately below one database root."""

    root_bytes = os.fspath(root).encode("utf-8", errors="strict")
    root_pointer, root_owner = _bytes_pointer(root_bytes)
    request = s.ZeNamespaceListRequest(
        abi_size=ct.sizeof(s.ZeNamespaceListRequest),
        abi_reserved=0,
        root=root_pointer,
        root_len=len(root_bytes),
    )
    result = s.sized(s.ZeNamespaceListResult)
    primary: BaseException | None = None
    try:
        status = LIBRARY.ze_namespace_list(ct.byref(request), ct.byref(result))
        raise_for_status(status)
        return [
            ct.string_at(result.entries[index].name, result.entries[index].name_len).decode(
                "utf-8"
            )
            for index in range(result.entry_count)
        ]
    except BaseException as error:
        primary = error
        raise
    finally:
        del root_owner
        free_status = LIBRARY.ze_namespace_list_result_free(ct.byref(result))
        if free_status != 0:
            if primary is None:
                raise_for_status(free_status)
            assert primary is not None
            primary.add_note(
                f"ze_namespace_list_result_free: {last_error_message()}"
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


def open_text(
    path: str | Path,
    bundle_path: str | Path,
    *,
    access_mode: AccessMode | str = AccessMode.READ_WRITE,
    durability: DurabilityMode | str = DurabilityMode.DERIVED,
    commit_tier: CommitTier | str = CommitTier.ORDERED,
    reader_drain_timeout_ms: int = 250,
    max_resident_bytes: int = (1 << 64) - 1,
    max_temp_bytes: int = (1 << 64) - 1,
) -> Store:
    """Open a text store and immutable model bundle through the text ABI."""
    request, path_owner = _open_request(
        path,
        access_mode,
        durability,
        commit_tier,
        reader_drain_timeout_ms,
        max_resident_bytes,
        max_temp_bytes,
    )
    bundle_pointer, bundle_len, bundle_owner = _text_pointer(str(bundle_path))
    text_request = s.ZeTextOpenRequest(
        abi_size=ct.sizeof(s.ZeTextOpenRequest),
        abi_reserved=0,
        store=request,
        bundle_path=bundle_pointer,
        bundle_path_len=bundle_len,
    )
    handle = ct.c_uint64()
    status = LIBRARY.ze_text_open(ct.byref(text_request), ct.byref(handle))
    del path_owner, bundle_owner
    raise_for_status(status, int(handle.value))
    return Store(int(handle.value), text=True)
