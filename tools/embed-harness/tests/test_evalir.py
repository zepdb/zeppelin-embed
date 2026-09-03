import tempfile
from pathlib import Path

import numpy as np

from embed_harness.evalir import encode_beir, load_beir
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
