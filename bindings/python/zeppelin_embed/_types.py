"""Python value types for the Zeppelin Embed ABI."""

from __future__ import annotations

from dataclasses import dataclass
from enum import IntEnum
from typing import TYPE_CHECKING, Any

if TYPE_CHECKING:
    import numpy as np


class AccessMode(IntEnum):
    READ_WRITE = 0
    READ_ONLY = 1


class DurabilityMode(IntEnum):
    DERIVED = 0
    DURABLE = 1
    ATTACHED = 2


class CommitTier(IntEnum):
    NONE = 0
    ORDERED = 1
    DURABLE = 2


class Tier(IntEnum):
    AUTO = 0
    EXACT = 1
    SCAN = 2
    GRAPH = 3


class GraphProfile(IntEnum):
    SIFT_CLASS = 0
    ANGULAR = 1


class StoreState(IntEnum):
    OPEN = 0
    CLOSING = 1
    CLOSED = 2


class QueryMode(IntEnum):
    VECTOR = 0
    LEXICAL = 1
    HYBRID = 2


class TextLegs(IntEnum):
    DENSE = 0
    LEXICAL = 1
    HYBRID = 2


class FusionMethod(IntEnum):
    CONVEX_COMBINATION = 0
    RECIPROCAL_RANK = 1


class MaintenanceStatus(IntEnum):
    COMPLETE = 0
    BUDGET_EXHAUSTED = 1


class Normalization(IntEnum):
    NONE = 0
    UNIT_L2 = 1


class EmbeddingRuntime(IntEnum):
    CORE_ML = 1
    MLX = 2
    CPU_REFERENCE = 3


class ComputeUnits(IntEnum):
    CPU = 1
    CPU_AND_GPU = 2
    CPU_AND_NEURAL_ENGINE = 3
    ALL = 4


class AttributeType(IntEnum):
    U64 = 1
    I64 = 2
    F64 = 3
    BOOL = 4
    DICTIONARY_STRING = 5
    RAW_STRING = 6


@dataclass(frozen=True)
class AttributeDefinition:
    attribute_id: int
    name: str
    attribute_type: AttributeType
    nullable: bool = False


@dataclass(frozen=True)
class AttributeValue:
    attribute_id: int
    attribute_type: AttributeType
    value: int | float | bool | str | None


@dataclass(frozen=True)
class VectorSpace:
    dimensions: int
    normalization: Normalization = Normalization.NONE
    epoch: EmbeddingEpoch | None = None


@dataclass(frozen=True)
class NamespaceSpec:
    attributes: tuple[AttributeDefinition, ...] = ()
    vector_space: VectorSpace | None = None


@dataclass(frozen=True)
class StoredDocument:
    doc_id: int | tuple[int, int]
    revision: int = 1
    timestamp: int = 0
    vector: np.ndarray[Any, Any] | None = None
    text: str | None = None
    metadata: bytes | None = None
    attributes: tuple[AttributeValue, ...] | None = None


@dataclass(frozen=True)
class GetResult:
    documents: tuple[StoredDocument | None, ...]
    missing_count: int
    generation: int


@dataclass(frozen=True)
class ScanCursor:
    _generation: int
    _segment_id: bytes
    _next_row: int
    _phase: int


@dataclass(frozen=True)
class ScanPage:
    documents: tuple[StoredDocument, ...]
    cursor: ScanCursor | None
    generation: int


@dataclass(frozen=True)
class CountResult:
    count: int
    generation: int


class Filter:
    """Build structured-filter dictionaries for record-store calls."""

    @staticmethod
    def eq(field: str | int, value: object) -> dict[str, Any]:
        return {"op": "eq", "field": field, "value": value}

    @staticmethod
    def not_eq(field: str | int, value: object) -> dict[str, Any]:
        return {"op": "not_eq", "field": field, "value": value}

    @staticmethod
    def in_(field: str | int, values: list[object]) -> dict[str, Any]:
        return {"op": "in", "field": field, "values": values}

    @staticmethod
    def not_in(field: str | int, values: list[object]) -> dict[str, Any]:
        return {"op": "not_in", "field": field, "values": values}

    @staticmethod
    def range_(
        field: str | int,
        *,
        gte: object | None = None,
        lte: object | None = None,
        gt: object | None = None,
        lt: object | None = None,
    ) -> dict[str, Any]:
        result: dict[str, Any] = {"op": "range", "field": field}
        for name, value in (("gte", gte), ("lte", lte), ("gt", gt), ("lt", lt)):
            if value is not None:
                result[name] = value
        return result

    @staticmethod
    def exists(field: str | int) -> dict[str, Any]:
        return {"op": "exists", "field": field}

    @staticmethod
    def is_null(field: str | int) -> dict[str, Any]:
        return {"op": "is_null", "field": field}

    @staticmethod
    def and_(*filters: dict[str, Any]) -> dict[str, Any]:
        return {"op": "and", "filters": list(filters)}

    @staticmethod
    def or_(*filters: dict[str, Any]) -> dict[str, Any]:
        return {"op": "or", "filters": list(filters)}

    @staticmethod
    def not_(filter: dict[str, Any]) -> dict[str, Any]:
        return {"op": "not", "filter": filter}


@dataclass(frozen=True)
class EmbeddingTower:
    model_id: str
    model_version: str
    weights_digest: bytes
    dims: int
    normalization: Normalization = Normalization.NONE
    prompt_prefix: str = ""
    max_tokens: int = 0
    runtime: EmbeddingRuntime = EmbeddingRuntime.CPU_REFERENCE
    compute_units: ComputeUnits = ComputeUnits.CPU
    os_build: str | None = None


@dataclass(frozen=True)
class EmbeddingEpoch:
    document: EmbeddingTower
    query: EmbeddingTower
    alignment_digest: bytes = b""
    tokenizer_profile: int = 0


@dataclass(frozen=True)
class EpochIdentity:
    embedding_epoch: int
    tokenizer_epoch: int


@dataclass(frozen=True)
class EpochAliasReport:
    generation: int
    previous_embedding_epoch: int
    previous_tokenizer_epoch: int
    published_embedding_epoch: int
    published_tokenizer_epoch: int
    manifest_committed: bool


@dataclass(frozen=True)
class EpochDropReport:
    generation: int
    segments_dropped: int
    bytes_reclaimed: int


@dataclass(frozen=True)
class StateReport:
    state: StoreState


@dataclass(frozen=True)
class StatsReport:
    resident_owned_bytes: int
    mapped_bytes: int
    mapped_resident_bytes: int
    segment_bytes: int
    active_segment_bytes: int
    active_row_count: int
    tombstone_count: int
    tombstone_bytes: int
    wal_bytes: int
    cache_bytes: int
    temporary_bytes: int
    query_pool_bytes: int
    open_files: int
    active_queries: int
    active_snapshot_leases: int
    phys_footprint: int | None


@dataclass(frozen=True)
class MutationReport:
    sequence: int
    generation: int


@dataclass(frozen=True)
class SearchHit:
    source_kind: int
    segment_id: bytes
    local_row: int
    doc_id: int | None
    revision: int | None
    score: float


@dataclass(frozen=True)
class SearchResult:
    hits: tuple[SearchHit, ...]
    generation: int
    dims_touched: int
    bytes_read: int
    threads_used: int
    graph_segments_traversed: int
    graph_validations: int
    graph_entry_seed_discoveries: int
    graph_visited_epoch_clears: int
    graph_candidates_scored: int
    graph_candidates_rescored: int
    graph_segments_pruned_by_bound: int


@dataclass(frozen=True)
class QueryHit:
    doc_id: int | None
    revision: int | None
    score: float
    vector_squared_l2: float | None
    lexical_bm25: float | None


@dataclass(frozen=True)
class TextHit:
    doc_id: int
    revision: int
    chunk: int
    text: str
    score: float
    vector_squared_l2: float | None
    lexical_bm25: float | None


@dataclass(frozen=True)
class TextQueryResult:
    hits: tuple[TextHit, ...]
    embedding_epoch: int
    tokenizer_epoch: int


@dataclass(frozen=True)
class FusionReport:
    method: FusionMethod
    effective_alpha: float
    rounds: int


@dataclass(frozen=True)
class QueryResult:
    hits: tuple[QueryHit, ...]
    generation: int
    mode: QueryMode
    approximate: bool
    exact_rescore: bool
    budget_exhausted: bool
    fusion: FusionReport | None
    embedding_epoch: int | None
    tokenizer_epoch: int | None
    dims_touched: int
    bytes_read: int
    docs_evaluated: int
    postings_decoded: int


@dataclass(frozen=True)
class GenerationReport:
    generation: int


@dataclass(frozen=True)
class PartitionReport:
    generation: int
    segments_dropped: int
    bytes_reclaimed: int
    straddlers_skipped: int
    is_no_op: bool


@dataclass(frozen=True)
class PurgeTokenReport:
    token_id: int
    generation: int
    unknown_id_count: int
    is_no_op: bool


@dataclass(frozen=True)
class PurgeReport:
    generation: int
    segments_rewritten: int
    unknown_id_count: int
    wal_rewritten: bool
    is_no_op: bool


@dataclass(frozen=True)
class MaintainReport:
    graphs_built: int
    bytes_consumed: int
    checkpoints_resumed: int
    status: MaintenanceStatus
