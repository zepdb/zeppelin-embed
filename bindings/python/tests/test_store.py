"""End-to-end tests for the Python Store surface."""

from __future__ import annotations

from pathlib import Path

import numpy as np
import pytest

import zeppelin_embed as ze


def _epoch(dims: int = 4) -> ze.EmbeddingEpoch:
    document = ze.EmbeddingTower(
        model_id="fixture-document",
        model_version="1",
        weights_digest=b"document-weights",
        dims=dims,
        max_tokens=128,
    )
    query = ze.EmbeddingTower(
        model_id="fixture-query",
        model_version="1",
        weights_digest=b"query-weights",
        dims=dims,
        max_tokens=128,
    )
    return ze.EmbeddingEpoch(
        document=document,
        query=query,
        alignment_digest=b"fixed-alignment",
    )


def test_open_ingest_query_close_round_trip(tmp_path: Path) -> None:
    vectors = np.asarray(
        [
            [0.125, 0.25, 0.375, 0.5],
            [0.25, 0.375, 0.5, 0.625],
            [0.75, 0.625, 0.5, 0.375],
        ],
        dtype=np.float32,
    )
    with ze.open(tmp_path / "store") as store:
        mutation = store.ingest(
            [1, 2, 3],
            vectors,
            revisions=[1, 1, 1],
            timestamps=[10, 20, 30],
            texts=["zeppelin harbour", "harbour lights", "vector index"],
            metadata=[b"one", None, b"three"],
        )
        assert mutation.sequence > 0
        assert mutation.generation > 0
        assert store.state().state == ze.StoreState.OPEN
        assert store.stats().active_row_count == 3

        searched = store.search(vectors[0], k=3, tier=None)
        assert [hit.doc_id for hit in searched.hits] == [3, 2, 1]
        assert searched.generation == mutation.generation

        lexical = store.query(text="harbour", k=3)
        assert lexical.mode == ze.QueryMode.LEXICAL
        assert [hit.doc_id for hit in lexical.hits] == [1, 2]
        assert all(hit.lexical_bm25 is not None for hit in lexical.hits)

        hybrid = store.query(vector=vectors[0], text="harbour", k=3)
        assert hybrid.mode == ze.QueryMode.HYBRID
        assert hybrid.exact_rescore
        assert hybrid.fusion is not None

        deleted = store.delete([3])
        assert deleted.sequence > mutation.sequence
        generation = store.seal()
        assert generation.generation >= deleted.generation

        partition = store.drop_partition(100, 200)
        assert partition.is_no_op
        retained = store.apply_retention(window=1_000, now_ts=30)
        assert retained.generation >= generation.generation

        scheduled = store.purge([(0xFFFF, 0xFFFF)])
        assert scheduled.is_no_op
        purged = store.await_physical_purge(scheduled.token_id)
        assert purged.is_no_op

        maintained = store.maintain(wall_time_ns=0, bytes=0)
        assert maintained.status == ze.MaintenanceStatus.BUDGET_EXHAUSTED

        with ze.CancelToken() as token:
            token.cancel()
            assert token.cancelled

    assert store.closed
    with pytest.raises(ze.Closed):
        store.stats()


def test_query_last_as_prefix_defaults_off_and_finds_trailing_prefix(tmp_path: Path) -> None:
    vectors = np.asarray([[1.0, 0.0], [0.0, 1.0]], dtype=np.float32)
    with ze.open(tmp_path / "store") as store:
        store.ingest([1, 2], vectors, texts=["meeting notes", "other document"])
        assert store.query(text="mee").hits == ()
        result = store.query(text="mee", last_as_prefix=True)
        assert [hit.doc_id for hit in result.hits] == [1]


def test_numpy_validation_alignment_and_optional_tier_are_typed(tmp_path: Path) -> None:
    vectors = np.asarray([[1.0, 2.0, 3.0, 4.0]], dtype=np.float32)
    with ze.open(tmp_path / "store") as store:
        store.ingest([1], vectors, texts=["harbour"])
        before = store.abi_call_count
        with pytest.raises(ze.InvalidArgument):
            store.search(vectors[0].astype(np.float64))
        with pytest.raises(ze.InvalidArgument):
            store.search(vectors)
        noncontiguous = np.zeros(8, dtype=np.float32)[::2]
        with pytest.raises(ze.InvalidArgument):
            store.search(noncontiguous)
        backing = np.zeros(4 * 4 + 1, dtype=np.uint8)
        misaligned = np.ndarray((4,), dtype=np.float32, buffer=backing, offset=1)
        assert misaligned.flags.c_contiguous
        with pytest.raises(ze.InvalidArgument, match="aligned"):
            store.search(misaligned)
        assert store.abi_call_count == before

        assert store.query(text="harbour", tier=None).hits
        with pytest.raises(ze.InvalidArgument):
            store.query(text="harbour", tier=ze.Tier.AUTO)


def test_epoch_declared_store_and_transitions_are_bound(tmp_path: Path) -> None:
    epoch = _epoch()
    expected = ze.epoch_identity(epoch)
    vectors = np.asarray([[0.0, 0.25, 0.5, 0.75]], dtype=np.float32)
    with ze.open_with_epoch(tmp_path / "store", epoch) as store:
        assert store.epoch_current() == expected
        store.ingest([1], vectors, texts=["declared epoch"])
        queried = store.query(vector=vectors[0], text="epoch")
        assert queried.embedding_epoch == expected.embedding_epoch
        assert queried.tokenizer_epoch == expected.tokenizer_epoch
        store.seal()
        switched = store.epoch_switch_alias(epoch)
        assert not switched.manifest_committed
        with pytest.raises(ze.EpochPublished):
            store.epoch_drop(epoch)
