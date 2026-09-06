"""Python bindings for the namespace record-store ABI."""

from __future__ import annotations

import ctypes as ct
from pathlib import Path

import numpy as np
import pytest

import zeppelin_embed as ze
from zeppelin_embed import _structures as s
from zeppelin_embed._library import LIBRARY


def _vector_spec(*, nullable: bool = False) -> ze.NamespaceSpec:
    return ze.NamespaceSpec(
        attributes=(
            ze.AttributeDefinition(1, "rank", ze.AttributeType.U64, nullable=nullable),
        ),
        vector_space=ze.VectorSpace(2),
    )


def test_namespace_create_reopen_schema_mismatch_and_list(tmp_path: Path) -> None:
    spec = _vector_spec()
    with ze.open_namespace(tmp_path, "articles", spec):
        pass
    with ze.open_namespace(tmp_path, "articles", spec):
        pass

    with ze.open_namespace(tmp_path, "notes", ze.NamespaceSpec()):
        pass

    assert ze.list_namespaces(tmp_path) == ["articles", "notes"]
    with pytest.raises(ze.SchemaMismatch):
        ze.open_namespace(tmp_path, "articles", _vector_spec(nullable=True))


def _all_types_spec() -> ze.NamespaceSpec:
    return ze.NamespaceSpec(
        attributes=(
            ze.AttributeDefinition(1, "u64", ze.AttributeType.U64),
            ze.AttributeDefinition(2, "i64", ze.AttributeType.I64),
            ze.AttributeDefinition(3, "f64", ze.AttributeType.F64),
            ze.AttributeDefinition(4, "bool", ze.AttributeType.BOOL),
            ze.AttributeDefinition(5, "tag", ze.AttributeType.DICTIONARY_STRING),
            ze.AttributeDefinition(6, "raw", ze.AttributeType.RAW_STRING),
        ),
        vector_space=ze.VectorSpace(2),
    )


def _document(doc_id: int, revision: int = 1, timestamp: int = 10) -> ze.StoredDocument:
    return ze.StoredDocument(
        doc_id=doc_id,
        revision=revision,
        timestamp=timestamp,
        vector=np.asarray([float(doc_id), float(revision)], dtype=np.float32),
        text=f"document {doc_id} revision {revision}",
        metadata=bytes([doc_id, revision]),
        attributes=(
            ze.AttributeValue(1, ze.AttributeType.U64, 42),
            ze.AttributeValue(2, ze.AttributeType.I64, -7),
            ze.AttributeValue(3, ze.AttributeType.F64, 1.25),
            ze.AttributeValue(4, ze.AttributeType.BOOL, True),
            ze.AttributeValue(5, ze.AttributeType.DICTIONARY_STRING, "blue"),
            ze.AttributeValue(6, ze.AttributeType.RAW_STRING, "raw value"),
        ),
    )


def test_upsert_every_attribute_type_round_trips_through_get(tmp_path: Path) -> None:
    with ze.open_namespace(tmp_path, "typed", _all_types_spec()) as store:
        report = store.upsert([_document(1)])
        result = store.get([1])

    assert report.generation == result.generation
    assert result.missing_count == 0
    document = result.documents[0]
    assert document is not None
    assert document.doc_id == 1
    assert document.revision == 1
    assert document.timestamp == 10
    np.testing.assert_array_equal(document.vector, np.asarray([1.0, 1.0], dtype=np.float32))
    assert document.text == "document 1 revision 1"
    assert document.metadata == b"\x01\x01"
    assert document.attributes == _document(1).attributes


def test_get_reports_missing_tombstoned_and_latest_revision_with_exact_fields(
    tmp_path: Path,
) -> None:
    with ze.open_namespace(tmp_path, "get", _all_types_spec()) as store:
        store.upsert([_document(1), _document(2)])
        store.upsert([_document(1, revision=2, timestamp=20)])
        store.delete([2])

        result = store.get([1, 99, 2])
        assert result.missing_count == 2
        assert result.documents[1:] == (None, None)
        current = result.documents[0]
        assert current is not None
        assert current.revision == 2
        assert current.timestamp == 20

        flags = ("vector", "text", "metadata", "attributes")
        for selected in flags:
            kwargs = {field: field == selected for field in flags}
            selected_result = store.get([1], **kwargs)
            selected_document = selected_result.documents[0]
            assert selected_document is not None
            for field in flags:
                assert (getattr(selected_document, field) is not None) == (field == selected)


def _scan_document(doc_id: int, timestamp: int) -> ze.StoredDocument:
    return ze.StoredDocument(
        doc_id=doc_id,
        timestamp=timestamp,
        vector=np.asarray([float(doc_id)], dtype=np.float32),
    )


def test_scan_orders_cross_segment_boundary_and_iterator_visits_each_live_row_once(
    tmp_path: Path,
) -> None:
    spec = ze.NamespaceSpec(vector_space=ze.VectorSpace(1))
    with ze.open_namespace(tmp_path, "scan", spec) as store:
        store.upsert([_scan_document(3, 20), _scan_document(2, 10)])
        store.seal()
        store.upsert([_scan_document(1, 20), _scan_document(4, 30)])

        first = store.scan(limit=2, vector=False, text=False, metadata=False, attributes=False)
        assert first.cursor is not None
        second = store.scan(
            cursor=first.cursor,
            limit=2,
            vector=False,
            text=False,
            metadata=False,
            attributes=False,
        )
        assert second.cursor is None
        assert [doc.doc_id for doc in first.documents + second.documents] == [3, 2, 1, 4]

        ascending = store.scan(order="timestamp_ascending", limit=10)
        descending = store.scan(order="timestamp_descending", limit=10)
        assert [doc.doc_id for doc in ascending.documents] == [2, 1, 3, 4]
        assert [doc.doc_id for doc in descending.documents] == [4, 1, 3, 2]

        iterated = list(store.iter_documents(vector=False, text=False, metadata=False, attributes=False))
        assert [doc.doc_id for doc in iterated] == [3, 2, 1, 4]
        assert len({doc.doc_id for doc in iterated}) == 4


def test_stale_scan_cursor_raises_without_restarting(tmp_path: Path) -> None:
    spec = ze.NamespaceSpec(vector_space=ze.VectorSpace(1))
    with ze.open_namespace(tmp_path, "stale", spec) as store:
        store.upsert([_scan_document(1, 1), _scan_document(2, 2)])
        page = store.scan(limit=1)
        assert page.cursor is not None
        store.upsert([_scan_document(3, 3)])
        store.seal()

        with pytest.raises(ze.ScanStale):
            store.scan(cursor=page.cursor, limit=1)


def _ranked_document(doc_id: int, rank: int) -> ze.StoredDocument:
    return ze.StoredDocument(
        doc_id=doc_id,
        timestamp=doc_id * 10,
        vector=np.asarray([float(doc_id)], dtype=np.float32),
        attributes=(ze.AttributeValue(1, ze.AttributeType.U64, rank),),
    )


def test_count_filter_matches_scan_and_filtered_search_matches_exact_postfilter(
    tmp_path: Path,
) -> None:
    spec = ze.NamespaceSpec(
        attributes=(ze.AttributeDefinition(1, "rank", ze.AttributeType.U64),),
        vector_space=ze.VectorSpace(1),
    )
    with ze.open_namespace(tmp_path, "filtered", spec) as store:
        store.upsert(
            [
                _ranked_document(1, 7),
                _ranked_document(2, 8),
                _ranked_document(3, 7),
                _ranked_document(4, 8),
            ]
        )
        selected = ze.Filter.and_(
            ze.Filter.in_("rank", [7]),
            ze.Filter.range_("rank", gte=7, lt=8),
            ze.Filter.exists("rank"),
            ze.Filter.not_(ze.Filter.is_null("rank")),
            ze.Filter.not_eq("rank", 8),
            ze.Filter.not_in("rank", [8]),
            ze.Filter.or_(ze.Filter.eq("rank", 7)),
        )
        scanned = store.scan(filter=selected, limit=10)
        counted = store.count(filter=selected)
        assert counted.count == len(scanned.documents) == 2
        assert counted.generation == scanned.generation

        query = np.asarray([0.0], dtype=np.float32)
        exact = store.search(query, k=4, tier=ze.Tier.EXACT)
        expected = [hit for hit in exact.hits if hit.doc_id in {1, 3}]
        filtered = store.search(
            query,
            k=4,
            tier=ze.Tier.EXACT,
            filter=ze.Filter.eq("rank", 7),
        )
        assert [(hit.doc_id, hit.score) for hit in filtered.hits] == [
            (hit.doc_id, hit.score) for hit in expected
        ]


def test_record_only_namespace_supports_records_and_rejects_vector_search(
    tmp_path: Path,
) -> None:
    with ze.open_namespace(tmp_path, "records", ze.NamespaceSpec()) as store:
        store.upsert(
            [
                ze.StoredDocument(
                    doc_id=7,
                    revision=2,
                    timestamp=9,
                    text="record",
                    metadata=b"metadata",
                )
            ]
        )
        fetched = store.get([7]).documents[0]
        scanned = store.scan(limit=10).documents[0]
        assert fetched is not None
        assert fetched.vector is None
        assert scanned.vector is None
        assert fetched.text == scanned.text == "record"
        assert fetched.metadata == scanned.metadata == b"metadata"

        with pytest.raises(ze.NoVectorSpace):
            store.search(np.asarray([1.0], dtype=np.float32))


def _raw_scan_status(store: ze.Store, nodes: list[s.ZeFilterNode], root: int) -> int:
    node_array = (s.ZeFilterNode * len(nodes))(*nodes)
    filter_value = s.ZeFilter(
        abi_size=ct.sizeof(s.ZeFilter),
        nodes=ct.cast(node_array, ct.POINTER(s.ZeFilterNode)),
        node_count=len(node_array),
        root=root,
    )
    request = s.ZeScanRequest(
        abi_size=ct.sizeof(s.ZeScanRequest),
        limit=1,
        filter=ct.pointer(filter_value),
    )
    result = s.sized(s.ZeScanResult)
    return int(LIBRARY.ze_scan(store._live_handle(), ct.byref(request), ct.byref(result)))


def test_malformed_filter_bad_index_cycle_and_depth_raise_typed_errors(
    tmp_path: Path,
) -> None:
    spec = ze.NamespaceSpec(
        attributes=(ze.AttributeDefinition(1, "rank", ze.AttributeType.U64),),
        vector_space=ze.VectorSpace(1),
    )
    leaf = s.ZeFilterNode(op=6, attribute_id=1)
    cycle = s.ZeFilterNode(op=10, children_start=0, children_count=1)
    depth = [s.ZeFilterNode(op=10, children_start=index + 1, children_count=1) for index in range(32)]
    depth.append(leaf)

    with ze.open_namespace(tmp_path, "malformed", spec) as store:
        store.upsert([_ranked_document(1, 7)])
        for nodes, root in [([leaf], 1), ([cycle], 0), (depth, 0)]:
            with pytest.raises(ze.InvalidArgument):
                ze.raise_for_status(_raw_scan_status(store, nodes, root), store._live_handle())
        cyclic_filter = ze.Filter.and_()
        cyclic_filter["filters"].append(cyclic_filter)
        with pytest.raises(ze.InvalidArgument):
            store.scan(filter=cyclic_filter)
        deep_filter = ze.Filter.exists("rank")
        for _ in range(32):
            deep_filter = ze.Filter.not_(deep_filter)
        with pytest.raises(ze.InvalidArgument):
            store.scan(filter=deep_filter)
