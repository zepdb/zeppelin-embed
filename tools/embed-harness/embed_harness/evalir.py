"""BEIR loading, exact dense evaluation, and hybrid-alpha integration."""

from __future__ import annotations

import argparse
import heapq
import hashlib
import json
import math
import os
import re
import subprocess
from collections.abc import Callable
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


def build_beir_subset(
    source_dir: Path,
    output_dir: Path,
    *,
    max_documents: int,
    seed: int = 20260903,
) -> dict[str, Any]:
    """Write a deterministic BEIR subset that retains all positive qrels."""
    if max_documents <= 0:
        raise ValueError("max_documents must be positive")
    corpus_path = source_dir / "corpus.jsonl"
    queries_path = source_dir / "queries.jsonl"
    qrels_path = source_dir / "qrels" / "test.tsv"
    for path in (corpus_path, queries_path, qrels_path):
        if not path.is_file():
            raise FileNotFoundError(f"required BEIR file is absent: {path}")

    qrel_rows: list[tuple[str, str, str]] = []
    relevant_ids = set()
    with qrels_path.open(encoding="utf-8") as source:
        header = source.readline().rstrip("\n").split("\t")
        if header != ["query-id", "corpus-id", "score"]:
            raise ValueError(f"unexpected qrels header in {qrels_path}: {header}")
        for line in source:
            query_id, document_id, score = line.rstrip("\n").split("\t")
            qrel_rows.append((query_id, document_id, score))
            if int(score) > 0:
                relevant_ids.add(document_id)
    if len(relevant_ids) > max_documents:
        raise ValueError(
            f"{len(relevant_ids)} relevant documents exceed the "
            f"{max_documents} document limit"
        )

    sample_count = max_documents - len(relevant_ids)
    sampled: list[tuple[int, int, str, str]] = []
    relevant_rows: list[tuple[int, str, str]] = []
    with corpus_path.open(encoding="utf-8") as source:
        for ordinal, line in enumerate(source):
            document_id = str(json.loads(line)["_id"])
            if document_id in relevant_ids:
                relevant_rows.append((ordinal, line, document_id))
                continue
            if sample_count == 0:
                continue
            rank = int.from_bytes(
                hashlib.sha256(f"{seed}\0{document_id}".encode()).digest(), "big"
            )
            item = (-rank, -ordinal, line, document_id)
            if len(sampled) < sample_count:
                heapq.heappush(sampled, item)
            elif item > sampled[0]:
                heapq.heapreplace(sampled, item)

    found_relevant = {document_id for _, _, document_id in relevant_rows}
    missing_relevant = relevant_ids - found_relevant
    if missing_relevant:
        example = sorted(missing_relevant)[0]
        raise ValueError(
            f"qrels reference {len(missing_relevant)} absent documents; "
            f"first is {example}"
        )
    selected = relevant_rows + [
        (-negative_ordinal, line, document_id)
        for _, negative_ordinal, line, document_id in sampled
    ]
    selected.sort(key=lambda row: row[0])
    selected_ids = {document_id for _, _, document_id in selected}
    retained_qrels = [row for row in qrel_rows if row[1] in selected_ids]
    retained_query_ids = {query_id for query_id, _, _ in retained_qrels}

    output_dir.mkdir(parents=True, exist_ok=True)
    (output_dir / "qrels").mkdir(parents=True, exist_ok=True)
    output_corpus = output_dir / "corpus.jsonl"
    output_queries = output_dir / "queries.jsonl"
    output_qrels = output_dir / "qrels" / "test.tsv"
    with output_corpus.open("w", encoding="utf-8") as output:
        for _, line, _ in selected:
            output.write(line if line.endswith("\n") else line + "\n")
    query_count = 0
    with queries_path.open(encoding="utf-8") as source, output_queries.open(
        "w", encoding="utf-8"
    ) as output:
        for line in source:
            if str(json.loads(line)["_id"]) in retained_query_ids:
                output.write(line if line.endswith("\n") else line + "\n")
                query_count += 1
    with output_qrels.open("w", encoding="utf-8") as output:
        output.write("query-id\tcorpus-id\tscore\n")
        for query_id, document_id, score in retained_qrels:
            output.write(f"{query_id}\t{document_id}\t{score}\n")

    manifest: dict[str, Any] = {
        "schema_version": "beir-subset-v1",
        "seed": seed,
        "max_documents": max_documents,
        "documents": len(selected),
        "relevant_documents": len(relevant_rows),
        "sampled_documents": len(selected) - len(relevant_rows),
        "queries": query_count,
        "qrels": len(retained_qrels),
        "source": {
            "corpus_sha256": _sha256(corpus_path),
            "queries_sha256": _sha256(queries_path),
            "qrels_sha256": _sha256(qrels_path),
        },
        "outputs": {
            "corpus_sha256": _sha256(output_corpus),
            "queries_sha256": _sha256(output_queries),
            "qrels_sha256": _sha256(output_qrels),
        },
    }
    (output_dir / "subset-manifest.json").write_text(
        json.dumps(manifest, indent=2, sort_keys=True) + "\n", encoding="utf-8"
    )
    return manifest


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


def _encode_batches(
    texts: list[str], batch_size: int, encode: Callable[[list[str]], np.ndarray]
) -> np.ndarray:
    if batch_size <= 0:
        raise ValueError("batch_size must be positive")
    batches = [
        encode(texts[start : start + batch_size])
        for start in range(0, len(texts), batch_size)
    ]
    if not batches:
        raise ValueError("cannot encode an empty text collection")
    return np.concatenate(batches, axis=0)


def encode_beir(
    encoder: NumpyBertEncoder,
    corpus: BeirCorpus,
    output: Path,
    *,
    batch_size: int = 32,
) -> dict[str, float]:
    corpus_vectors = _encode_batches(
        [document.combined_text for document in corpus.documents],
        batch_size,
        encoder.encode_documents,
    )
    query_vectors = _encode_batches(
        [query.text for query in corpus.queries], batch_size, encoder.encode_texts
    )
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
    encode.add_argument("--batch-size", type=int, default=32)
    hybrid = subparsers.add_parser("hybrid")
    hybrid.add_argument("--binary", default="target/release/hybrid-alpha", type=Path)
    hybrid.add_argument("--beir-dir", required=True, type=Path)
    hybrid.add_argument("--corpus", required=True)
    hybrid.add_argument("--vectors", required=True, type=Path)
    hybrid.add_argument("--k", default=10, type=int)
    hybrid.add_argument("--time-searches", action="store_true")
    hybrid.add_argument("--out", required=True, type=Path)
    subset = subparsers.add_parser("subset-beir")
    subset.add_argument("--source", required=True, type=Path)
    subset.add_argument("--out", required=True, type=Path)
    subset.add_argument("--max-documents", required=True, type=int)
    subset.add_argument("--seed", type=int, default=20260903)
    args = parser.parse_args()
    if args.command == "encode-beir":
        encoder = (
            MlxBertEncoder(args.model)
            if args.backend == "mlx"
            else NumpyBertEncoder(args.model)
        )
        result = encode_beir(
            encoder,
            load_beir(args.beir_dir / args.corpus),
            args.out,
            batch_size=args.batch_size,
        )
        print(json.dumps(result, sort_keys=True))
    elif args.command == "hybrid":
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
    else:
        result = build_beir_subset(
            args.source,
            args.out,
            max_documents=args.max_documents,
            seed=args.seed,
        )
        print(json.dumps(result, sort_keys=True))


if __name__ == "__main__":
    main()
