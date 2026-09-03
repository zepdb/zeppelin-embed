"""P1 word-order and P2 NevIR negation probes."""

from __future__ import annotations

import argparse
import csv
import json
from collections.abc import Callable
from dataclasses import dataclass
from pathlib import Path

import numpy as np

from .encode import MlxBertEncoder, NumpyBertEncoder
from .evalir import dense_retrieval, load_beir, mean_ndcg_at_k
from .towers import encode_q4

SEED = 20260903


def shuffle_token_rows(
    token_rows: list[list[int]], seed: int = SEED
) -> list[list[int]]:
    rng = np.random.default_rng(seed)
    shuffled = []
    for row in token_rows:
        permutation = rng.permutation(len(row))
        shuffled.append([row[int(index)] for index in permutation])
    return shuffled


def _padded_rows(
    encoder: NumpyBertEncoder, token_rows: list[list[int]]
) -> tuple[np.ndarray, np.ndarray]:
    if not token_rows or any(not row for row in token_rows):
        raise ValueError("transformer token rows must be non-empty")
    width = max(len(row) for row in token_rows)
    ids = np.full((len(token_rows), width), encoder.pad_token_id, dtype=np.int64)
    mask = np.zeros((len(token_rows), width), dtype=np.float32)
    for index, row in enumerate(token_rows):
        ids[index, : len(row)] = row
        mask[index, : len(row)] = 1.0
    return ids, mask


def encode_transformer_token_rows(
    encoder: NumpyBertEncoder, token_rows: list[list[int]]
) -> np.ndarray:
    return encoder.forward(*_padded_rows(encoder, token_rows))


def word_order_probe(
    encode_rows: Callable[[list[list[int]]], np.ndarray],
    token_rows: list[list[int]],
    document_vectors: np.ndarray,
    query_ids: list[str],
    document_ids: list[str],
    qrels: dict[str, dict[str, int]],
) -> dict[str, float]:
    original = encode_rows(token_rows)
    shuffled = encode_rows(shuffle_token_rows(token_rows))
    original_run = dense_retrieval(
        original, document_vectors, query_ids, document_ids, k=10
    )
    shuffled_run = dense_retrieval(
        shuffled, document_vectors, query_ids, document_ids, k=10
    )
    original_ndcg = mean_ndcg_at_k(original_run, qrels, 10)
    shuffled_ndcg = mean_ndcg_at_k(shuffled_run, qrels, 10)
    return {
        "original_ndcg10": float(original_ndcg),
        "shuffled_ndcg10": float(shuffled_ndcg),
        "delta_ndcg10": float(shuffled_ndcg - original_ndcg),
    }


@dataclass(frozen=True)
class NevirPair:
    query_a: str
    query_b: str
    doc_a: str
    doc_b: str


def load_nevir(path: Path) -> list[NevirPair]:
    if not path.is_file():
        raise FileNotFoundError(f"NevIR data is absent: {path}")
    pairs = []
    if path.suffix == ".jsonl":
        with path.open(encoding="utf-8") as source:
            for line in source:
                row = json.loads(line)
                pairs.append(
                    NevirPair(
                        row["query_a"], row["query_b"], row["doc_a"], row["doc_b"]
                    )
                )
    elif path.suffix == ".tsv":
        with path.open(encoding="utf-8", newline="") as source:
            reader = csv.DictReader(source, delimiter="\t")
            expected = ["query_a", "query_b", "doc_a", "doc_b"]
            if reader.fieldnames != expected:
                raise ValueError(f"NevIR TSV header must be {expected}")
            for row in reader:
                pairs.append(
                    NevirPair(
                        row["query_a"], row["query_b"], row["doc_a"], row["doc_b"]
                    )
                )
    else:
        raise ValueError("NevIR path must end in .jsonl or .tsv")
    if not pairs:
        raise ValueError("NevIR data contains no pairs")
    return pairs


def nevir_pairwise_accuracy(
    encode_queries: Callable[[list[str]], np.ndarray],
    encode_documents: Callable[[list[str]], np.ndarray],
    pairs: list[NevirPair],
) -> float:
    queries = encode_queries(
        [text for pair in pairs for text in (pair.query_a, pair.query_b)]
    )
    documents = encode_documents(
        [text for pair in pairs for text in (pair.doc_a, pair.doc_b)]
    )
    correct = 0
    for index in range(len(pairs)):
        query_a, query_b = queries[index * 2 : index * 2 + 2]
        doc_a, doc_b = documents[index * 2 : index * 2 + 2]
        if float(query_a @ doc_a) > float(query_a @ doc_b) and float(
            query_b @ doc_b
        ) > float(query_b @ doc_a):
            correct += 1
    return float(correct / len(pairs))


class _Tower:
    def __init__(self, teacher_dir: Path, tower: str):
        self.teacher: NumpyBertEncoder
        self.kind = tower
        self.table = None
        self.weights = None
        self.layers = None
        if tower == "q1":
            self.teacher = NumpyBertEncoder(teacher_dir)
            return
        path = Path(tower)
        metadata = json.loads((path / "tower.json").read_text(encoding="utf-8"))
        archive = np.load(path / "tower.npz", allow_pickle=False)
        self.kind = metadata["kind"]
        if self.kind == "q4":
            self.teacher = NumpyBertEncoder(teacher_dir)
            self.table = archive["table"]
        else:
            mlx_teacher = MlxBertEncoder(teacher_dir)
            self.teacher = mlx_teacher
            self.weights = {
                name: mlx_teacher.mx.array(archive[name]) for name in archive.files
            }
            self.layers = metadata["layer_indices"]

    def token_rows(self, texts: list[str]) -> list[list[int]]:
        tokenize = (
            self.teacher.table_token_ids
            if self.kind == "q4"
            else self.teacher.token_ids
        )
        return [tokenize(text) for text in texts]

    def encode_rows(self, rows: list[list[int]]) -> np.ndarray:
        if self.kind == "q4":
            return encode_q4(self.table, rows)
        ids, mask = _padded_rows(self.teacher, rows)
        if self.kind == "q1":
            return self.teacher.forward(ids, mask)
        output = self.teacher.forward_mx(
            ids, mask, weights=self.weights, layer_indices=self.layers
        )
        self.teacher.mx.eval(output)
        return np.asarray(output)

    def encode_texts(self, texts: list[str]) -> np.ndarray:
        return self.encode_rows(self.token_rows(texts))


def _read_documents(vectors: Path) -> tuple[list[str], np.ndarray]:
    ids = (vectors / "corpus_ids.txt").read_text(encoding="utf-8").splitlines()
    meta = json.loads((vectors / "meta.json").read_text(encoding="utf-8"))
    flat = np.fromfile(vectors / "corpus_vectors.f32", dtype="<f4")
    expected = len(ids) * int(meta["dims"])
    if flat.size != expected:
        raise ValueError("corpus vector byte count does not match ids and dimensions")
    return ids, flat.reshape(len(ids), int(meta["dims"]))


def main() -> None:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--teacher", required=True, type=Path)
    parser.add_argument("--tower", required=True)
    parser.add_argument("--probe", required=True, choices=("shuffle", "nevir"))
    parser.add_argument("--beir", type=Path)
    parser.add_argument("--doc-vectors", type=Path)
    parser.add_argument("--nevir", type=Path)
    parser.add_argument("--out", required=True, type=Path)
    args = parser.parse_args()
    tower = _Tower(args.teacher, args.tower)
    if args.probe == "shuffle":
        if args.beir is None or args.doc_vectors is None:
            parser.error("shuffle requires --beir and --doc-vectors")
        corpus = load_beir(args.beir)
        document_ids, document_vectors = _read_documents(args.doc_vectors)
        report = word_order_probe(
            tower.encode_rows,
            tower.token_rows([query.text for query in corpus.queries]),
            document_vectors,
            corpus.query_ids,
            document_ids,
            corpus.qrels,
        )
    else:
        if args.nevir is None:
            parser.error("nevir requires --nevir")
        report = {
            "pairwise_accuracy": nevir_pairwise_accuracy(
                tower.encode_texts,
                NumpyBertEncoder(args.teacher).encode_documents,
                load_nevir(args.nevir),
            )
        }
    args.out.parent.mkdir(parents=True, exist_ok=True)
    args.out.write_text(
        json.dumps(report, indent=2, sort_keys=True) + "\n", encoding="utf-8"
    )


if __name__ == "__main__":
    main()
