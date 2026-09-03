"""BEIR loading, exact dense evaluation, and hybrid-alpha integration."""

from __future__ import annotations

import argparse
import hashlib
import json
import math
import os
import re
import subprocess
from dataclasses import dataclass
from pathlib import Path
from typing import Any

import numpy as np

from .encode import MlxBertEncoder, NumpyBertEncoder


@dataclass(frozen=True)
class Document:
    id: str
    title: str
    text: str

    @property
    def combined_text(self) -> str:
        return f"{self.title} {self.text}" if self.title else self.text


@dataclass(frozen=True)
class Query:
    id: str
    text: str


@dataclass(frozen=True)
class BeirCorpus:
    documents: list[Document]
    queries: list[Query]
    qrels: dict[str, dict[str, int]]

    @property
    def document_ids(self) -> list[str]:
        return [document.id for document in self.documents]

    @property
    def query_ids(self) -> list[str]:
        return [query.id for query in self.queries]


def load_beir(corpus_dir: Path) -> BeirCorpus:
    required = (
        corpus_dir / "corpus.jsonl",
        corpus_dir / "queries.jsonl",
        corpus_dir / "qrels" / "test.tsv",
    )
    for path in required:
        if not path.is_file():
            raise FileNotFoundError(f"required BEIR file is absent: {path}")
    documents = []
    with required[0].open(encoding="utf-8") as source:
        for line in source:
            row = json.loads(line)
            documents.append(
                Document(str(row["_id"]), str(row.get("title", "")), str(row["text"]))
            )
    queries = []
    with required[1].open(encoding="utf-8") as source:
        for line in source:
            row = json.loads(line)
            queries.append(Query(str(row["_id"]), str(row["text"])))
    qrels: dict[str, dict[str, int]] = {}
    with required[2].open(encoding="utf-8") as source:
        header = source.readline().rstrip("\n").split("\t")
        if header != ["query-id", "corpus-id", "score"]:
            raise ValueError(f"unexpected qrels header in {required[2]}: {header}")
        for line in source:
            query_id, document_id, score = line.rstrip("\n").split("\t")
            qrels.setdefault(query_id, {})[document_id] = int(score)
    return BeirCorpus(documents, queries, qrels)


def load_miracl(_path: Path) -> None:
    raise NotImplementedError("MIRACL: loader not implemented")


def ndcg_at_k(
    ranked: list[str], judgements: dict[str, int], k: int = 10
) -> float | None:
    positive = sorted(
        (grade for grade in judgements.values() if grade > 0), reverse=True
    )
    if not positive:
        return None

    def dcg(grades: list[int]) -> float:
        return sum(
            grade / math.log2(rank + 1.0) for rank, grade in enumerate(grades, 1)
        )

    ideal = dcg(positive[:k])
    retrieved = [judgements.get(document_id, 0) for document_id in ranked[:k]]
    return float(dcg(retrieved) / ideal)


def mean_ndcg_at_k(
    run: dict[str, list[str]], qrels: dict[str, dict[str, int]], k: int = 10
) -> float:
    scores = []
    for query_id, judgements in qrels.items():
        score = ndcg_at_k(run.get(query_id, []), judgements, k)
        if score is not None:
            scores.append(score)
    return float(sum(scores) / len(scores)) if scores else 0.0


def _normalized(matrix: np.ndarray) -> np.ndarray:
    values = np.asarray(matrix, dtype=np.float32)
    norms = np.linalg.norm(values, axis=1, keepdims=True)
    return values / np.maximum(norms, np.finfo(np.float32).tiny)


def dense_retrieval(
    query_vectors: np.ndarray,
    corpus_vectors: np.ndarray,
    query_ids: list[str],
    corpus_ids: list[str],
    *,
    k: int = 10,
) -> dict[str, list[str]]:
    queries = _normalized(query_vectors)
    corpus = _normalized(corpus_vectors)
    if len(queries) != len(query_ids) or len(corpus) != len(corpus_ids):
        raise ValueError("vector row counts must match their id files")
    if queries.shape[1] != corpus.shape[1]:
        raise ValueError("query and corpus dimensions differ")
    take = min(k, len(corpus_ids))
    run = {}
    for query_id, vector in zip(query_ids, queries, strict=True):
        scores = corpus @ vector
        if take == len(scores):
            candidates = np.arange(len(scores))
        else:
            candidates = np.argpartition(scores, -take)[-take:]
        ranked = candidates[np.lexsort((candidates, -scores[candidates]))]
        run[query_id] = [corpus_ids[int(index)] for index in ranked]
    return run


def write_ragbench_vectors(
    output: Path,
    corpus_vectors: np.ndarray,
    query_vectors: np.ndarray,
    corpus_ids: list[str],
    query_ids: list[str],
) -> None:
    corpus = _normalized(corpus_vectors).astype("<f4", copy=False)
    queries = _normalized(query_vectors).astype("<f4", copy=False)
    if corpus.shape[1] != queries.shape[1]:
        raise ValueError("query and corpus dimensions differ")
    output.mkdir(parents=True, exist_ok=True)
    (output / "meta.json").write_text(
        json.dumps({"dims": int(corpus.shape[1]), "metric": "cosine"}) + "\n",
        encoding="utf-8",
    )
    corpus.tofile(output / "corpus_vectors.f32")
    queries.tofile(output / "query_vectors.f32")
    (output / "corpus_ids.txt").write_text(
        "\n".join(corpus_ids) + "\n", encoding="utf-8"
    )
    (output / "query_ids.txt").write_text("\n".join(query_ids) + "\n", encoding="utf-8")


def _sha256(path: Path) -> str:
    digest = hashlib.sha256()
    with path.open("rb") as source:
        for chunk in iter(lambda: source.read(1024 * 1024), b""):
            digest.update(chunk)
    return digest.hexdigest()


def binary_identity(binary: Path) -> dict[str, Any]:
    if not binary.is_file():
        raise FileNotFoundError(f"hybrid-alpha binary is absent: {binary}")
    symbols = subprocess.run(
        ["nm", str(binary)], check=False, capture_output=True, text=True
    )
    return {
        "path": str(binary),
        "bytes": float(binary.stat().st_size),
        "sha256": _sha256(binary),
        "llvm_profile_symbols_absent": "__llvm_profile_" not in symbols.stdout,
    }


def invoke_hybrid_alpha(
    binary: Path,
    beir_dir: Path,
    corpus: str,
    vectors: Path,
    *,
    k: int = 10,
    time_searches: bool = False,
) -> dict[str, Any]:
    command = [
        str(binary),
        "--corpus",
        corpus,
        "--vectors",
        str(vectors),
        "--k",
        str(k),
    ]
    if time_searches:
        command.append("--time-searches")
    environment = os.environ.copy()
    environment["ZE_BEIR_DIR"] = str(beir_dir)
    completed = subprocess.run(
        command, check=True, capture_output=True, text=True, env=environment
    )
    cells = []
    best = None
    pattern = re.compile(
        r"^HYBRID_ALPHA_RESULT arm=(\S+) alpha=([0-9.]+) ndcg10=([0-9.]+)"
    )
    best_pattern = re.compile(r"^BEST_ALPHA alpha=([0-9.]+) ndcg10=([0-9.]+)")
    for line in completed.stdout.splitlines():
        match = pattern.match(line)
        if match:
            cells.append(
                {
                    "arm": match.group(1),
                    "alpha": float(match.group(2)),
                    "ndcg10": float(match.group(3)),
                }
            )
        match = best_pattern.match(line)
        if match:
            best = {"alpha": float(match.group(1)), "ndcg10": float(match.group(2))}
    if best is None:
        raise ValueError("hybrid-alpha emitted no BEST_ALPHA line")
    return {
        "cells": cells,
        "best": best,
        "binary": binary_identity(binary),
        "stdout": completed.stdout,
    }


DEFAULT_CATEGORIES = {
    "plain": ("scifact", "nfcorpus", "trec-covid", "fiqa"),
    "multihop_entity": ("hotpotqa", "dbpedia-entity", "scidocs"),
    "multilingual": ("miracl-ja", "miracl-de", "miracl-ar"),
}


def category_retention(
    measured: dict[str, float | None],
    q1_reference: dict[str, float | None],
    categories: dict[str, tuple[str, ...]] = DEFAULT_CATEGORIES,
) -> tuple[dict[str, float | None], list[str]]:
    retention = {}
    notes = []
    for category, corpora in categories.items():
        available = [
            corpus
            for corpus in corpora
            if measured.get(corpus) is not None and q1_reference.get(corpus) is not None
        ]
        missing = [corpus for corpus in corpora if corpus not in available]
        if missing:
            retention[category] = None
            notes.append(
                f"{category}: missing per-corpus numbers for {','.join(missing)}"
            )
            continue
        reference_mean = sum(float(q1_reference[corpus]) for corpus in available) / len(
            available
        )
        measured_mean = sum(float(measured[corpus]) for corpus in available) / len(
            available
        )
        retention[category] = (
            float(measured_mean / reference_mean) if reference_mean else None
        )
        if reference_mean == 0.0:
            notes.append(f"{category}: Q1 reference mean is zero")
    return retention, notes


def encode_beir(
    encoder: NumpyBertEncoder, corpus: BeirCorpus, output: Path
) -> dict[str, float]:
    corpus_vectors = encoder.encode_documents(
        [document.combined_text for document in corpus.documents]
    )
    query_vectors = encoder.encode_texts([query.text for query in corpus.queries])
    write_ragbench_vectors(
        output, corpus_vectors, query_vectors, corpus.document_ids, corpus.query_ids
    )
    run = dense_retrieval(
        query_vectors, corpus_vectors, corpus.query_ids, corpus.document_ids, k=10
    )
    return {"dense_ndcg10": mean_ndcg_at_k(run, corpus.qrels, 10)}


def main() -> None:
    parser = argparse.ArgumentParser(description=__doc__)
    subparsers = parser.add_subparsers(dest="command", required=True)
    encode = subparsers.add_parser("encode-beir")
    encode.add_argument("--model", required=True, type=Path)
    encode.add_argument("--backend", choices=("mlx", "numpy"), default="mlx")
    encode.add_argument("--beir-dir", required=True, type=Path)
    encode.add_argument("--corpus", required=True)
    encode.add_argument("--out", required=True, type=Path)
    hybrid = subparsers.add_parser("hybrid")
    hybrid.add_argument("--binary", default="target/release/hybrid-alpha", type=Path)
    hybrid.add_argument("--beir-dir", required=True, type=Path)
    hybrid.add_argument("--corpus", required=True)
    hybrid.add_argument("--vectors", required=True, type=Path)
    hybrid.add_argument("--k", default=10, type=int)
    hybrid.add_argument("--time-searches", action="store_true")
    hybrid.add_argument("--out", required=True, type=Path)
    args = parser.parse_args()
    if args.command == "encode-beir":
        encoder = (
            MlxBertEncoder(args.model)
            if args.backend == "mlx"
            else NumpyBertEncoder(args.model)
        )
        result = encode_beir(encoder, load_beir(args.beir_dir / args.corpus), args.out)
        print(json.dumps(result, sort_keys=True))
    else:
        result = invoke_hybrid_alpha(
            args.binary,
            args.beir_dir,
            args.corpus,
            args.vectors,
            k=args.k,
            time_searches=args.time_searches,
        )
        args.out.parent.mkdir(parents=True, exist_ok=True)
        args.out.write_text(
            json.dumps(result, indent=2, sort_keys=True) + "\n", encoding="utf-8"
        )


if __name__ == "__main__":
    main()
