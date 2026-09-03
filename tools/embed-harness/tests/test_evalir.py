import json
import tempfile
from pathlib import Path

import numpy as np

from embed_harness.evalir import build_beir_subset, encode_beir, load_beir
from tests.synthetic import write_beir


class RecordingEncoder:
    def __init__(self) -> None:
        self.document_batches: list[int] = []
        self.query_batches: list[int] = []

    def encode_documents(self, texts: list[str]) -> np.ndarray:
        self.document_batches.append(len(texts))
        return self._vectors(texts)

    def encode_texts(self, texts: list[str]) -> np.ndarray:
        self.query_batches.append(len(texts))
        return self._vectors(texts)

    @staticmethod
    def _vectors(texts: list[str]) -> np.ndarray:
        rows = []
        for text in texts:
            index = int(text.removeprefix("t"))
            rows.append([float(index + 1), 1.0])
        return np.asarray(rows, dtype=np.float32)


def a_beir_encoding_never_submits_more_than_the_requested_batch_size():
    with tempfile.TemporaryDirectory() as temporary:
        root = Path(temporary)
        corpus = load_beir(write_beir(root))
        encoder = RecordingEncoder()

        encode_beir(encoder, corpus, root / "vectors", batch_size=7)

        assert encoder.document_batches == [7, 7, 6]
        assert encoder.query_batches == [7, 7, 6]


def a_beir_subset_keeps_all_judged_documents_and_is_reproducible():
    with tempfile.TemporaryDirectory() as temporary:
        root = Path(temporary)
        source = write_beir(root)
        (source / "qrels" / "test.tsv").write_text(
            "query-id\tcorpus-id\tscore\n"
            "q0\td0\t1\n"
            "q1\td1\t1\n"
            "q2\td2\t1\n"
            "q3\td2\t0\n",
            encoding="utf-8",
        )

        first = build_beir_subset(
            source, root / "subset-first", max_documents=7, seed=20260903
        )
        second = build_beir_subset(
            source, root / "subset-second", max_documents=7, seed=20260903
        )

        corpus_ids = {
            json.loads(line)["_id"]
            for line in (root / "subset-first" / "corpus.jsonl")
            .read_text(encoding="utf-8")
            .splitlines()
        }
        query_ids = [
            json.loads(line)["_id"]
            for line in (root / "subset-first" / "queries.jsonl")
            .read_text(encoding="utf-8")
            .splitlines()
        ]
        assert first["documents"] == 7
        assert {"d0", "d1", "d2"} <= corpus_ids
        assert query_ids == ["q0", "q1", "q2", "q3"]
        assert first["outputs"] == second["outputs"]
