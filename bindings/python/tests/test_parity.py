"""Cross-binding parity fixture consumer."""

from __future__ import annotations

import json
from pathlib import Path
from typing import Any

import numpy as np
import pytest

import zeppelin_embed as ze

FIXTURE = Path(__file__).resolve().parents[2] / "fixtures" / "cross_binding_parity_v1.json"


def _tower(value: dict[str, Any]) -> ze.EmbeddingTower:
    return ze.EmbeddingTower(
        model_id=value["model_id"],
        model_version=value["model_version"],
        weights_digest=bytes.fromhex(value["weights_digest_hex"]),
        dims=value["dims"],
        normalization=ze.Normalization(value["normalization"]),
        prompt_prefix=value["prompt_prefix"],
        max_tokens=value["max_tokens"],
        runtime=ze.EmbeddingRuntime(value["runtime"]),
        compute_units=ze.ComputeUnits(value["compute_units"]),
        os_build=value["os_build"],
    )


def _doc_id(value: dict[str, int]) -> tuple[int, int]:
    return value["high"], value["low"]


def test_python_matches_rust_cross_binding_parity_fixture(tmp_path: Path) -> None:
    fixture = json.loads(FIXTURE.read_text(encoding="utf-8"))
    assert fixture["schema"] == "zeppelin-embed-cross-binding-parity"
    assert fixture["version"] == 1
    assert fixture["seed"] == 25_172_023
    assert fixture["score_precision"] == 6

    epoch_value = fixture["epoch"]
    epoch = ze.EmbeddingEpoch(
        document=_tower(epoch_value["document"]),
        query=_tower(epoch_value["query"]),
        alignment_digest=bytes.fromhex(epoch_value["alignment_digest_hex"]),
        tokenizer_profile=epoch_value["tokenizer_profile"],
    )
    assert ze.epoch_identity(epoch) == ze.EpochIdentity(**epoch_value["expected_identity"])

    operations = fixture["operations"]
    assert operations[0]["kind"] == "open_with_epoch"
    assert operations[0]["expected"]["error_code"] == "ZE_OK"
    with ze.open(tmp_path / "parity-store", epoch=epoch) as store:
        ingest = operations[1]
        documents = ingest["documents"]
        mutation = store.ingest(
            [_doc_id(document["doc_id"]) for document in documents],
            np.asarray([document["vector"] for document in documents], dtype=np.float32),
            revisions=[document["revision"] for document in documents],
            timestamps=[document["timestamp"] for document in documents],
            texts=[document["text"] for document in documents],
            metadata=[bytes.fromhex(document["metadata_hex"]) for document in documents],
        )
        assert ingest["expected"] == {
            "error_code": "ZE_OK",
            "sequence": mutation.sequence,
            "generation": mutation.generation,
        }

        for operation in operations[2:5]:
            request = operation["request"]
            vector = request["vector"]
            result = store.query(
                vector=None if vector is None else np.asarray(vector, dtype=np.float32),
                text=request["text"],
                k=request["k"],
                tier=None,
                rules_enabled=request["rules_enabled"],
            )
            expected = operation["expected"]
            assert expected["error_code"] == "ZE_OK"
            assert result.generation == expected["generation"]
            assert int(result.mode) == expected["mode"]
            assert result.embedding_epoch == expected["embedding_epoch"]
            assert result.tokenizer_epoch == expected["tokenizer_epoch"]
            assert len(result.hits) == len(expected["hits"])
            for hit, expected_hit in zip(result.hits, expected["hits"], strict=True):
                assert hit.doc_id == (
                    expected_hit["doc_id"]["high"] << 64
                    | expected_hit["doc_id"]["low"]
                )
                assert round(hit.score, 6) == expected_hit["score"]
                assert (
                    None
                    if hit.vector_squared_l2 is None
                    else round(hit.vector_squared_l2, 6)
                ) == expected_hit["vector_squared_l2"]
                assert (
                    None if hit.lexical_bm25 is None else round(hit.lexical_bm25, 6)
                ) == expected_hit["lexical_bm25"]

        invalid = operations[5]
        assert invalid["kind"] == "invalid_empty_query"
        with pytest.raises(ze.InvalidArgument) as caught:
            store.query()
        assert caught.value.code_name == invalid["expected"]["error_code"]
        assert store.epoch_current() == ze.EpochIdentity(**epoch_value["expected_identity"])

    assert operations[6] == {"kind": "close", "expected": {"error_code": "ZE_OK"}}
