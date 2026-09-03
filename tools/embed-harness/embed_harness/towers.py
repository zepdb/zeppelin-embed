"""Q1-Q4 query towers and one shared MLX distillation loop."""

from __future__ import annotations

import argparse
import hashlib
import json
import math
import time
from dataclasses import dataclass
from pathlib import Path
from typing import Any

import numpy as np

from .encode import MlxBertEncoder, NumpyBertEncoder
from .evalir import dense_retrieval, load_beir, mean_ndcg_at_k, write_ragbench_vectors

SEED = 20260903
DEFAULT_BATCH = 128
DEFAULT_LR = 1.0e-4
DEFAULT_WARMUP = 1_000
DEFAULT_TEMPERATURE = 0.05


def build_q4_table(teacher: NumpyBertEncoder, batch_size: int = 256) -> np.ndarray:
    """Run the frozen teacher on each vocabulary token with its query prefix."""
    prefix_ids = teacher.tokenizer.encode(teacher.prefix, add_special_tokens=False).ids
    rows = []
    for start in range(0, teacher.vocab_size, batch_size):
        token_rows = [
            teacher.wrap_special_tokens(prefix_ids + [token_id])
            for token_id in range(start, min(start + batch_size, teacher.vocab_size))
        ]
        width = max(len(row) for row in token_rows)
        ids = np.full((len(token_rows), width), teacher.pad_token_id, dtype=np.int64)
        mask = np.zeros((len(token_rows), width), dtype=np.float32)
        for index, row in enumerate(token_rows):
            ids[index, : len(row)] = row
            mask[index, : len(row)] = 1.0
        rows.append(teacher.forward(ids, mask))
    return np.concatenate(rows, axis=0).astype(np.float32)


def encode_q4(table: np.ndarray, token_rows: list[list[int]]) -> np.ndarray:
    """Canonical bag inference: sorted token ids, with duplicates retained."""
    encoded = []
    for row in token_rows:
        if not row:
            raise ValueError("Q4 cannot encode an empty token row")
        canonical = np.asarray(sorted(row), dtype=np.int64)
        pooled = table[canonical].mean(axis=0, dtype=np.float32)
        norm = float(np.linalg.norm(pooled))
        encoded.append(pooled / norm if norm > 0.0 else pooled)
    return np.stack(encoded).astype(np.float32)


def int8_table_size_bytes(table: np.ndarray) -> float:
    """One signed byte per value plus one little-endian f32 scale per row."""
    return float(table.size + table.shape[0] * 4)


def q1(teacher: NumpyBertEncoder, texts: list[str]) -> np.ndarray:
    return teacher.encode_texts(texts)


def q5(*_args: Any, **_kwargs: Any) -> None:
    raise NotImplementedError("Q5: owner-gated co-trained pair not implemented")


def load_multilingual_training_set(_path: Path) -> None:
    raise NotImplementedError("multilingual training set: reader not implemented")


def _sha256(path: Path) -> str:
    digest = hashlib.sha256()
    with path.open("rb") as source:
        for chunk in iter(lambda: source.read(1024 * 1024), b""):
            digest.update(chunk)
    return digest.hexdigest()


def write_training_manifest(files: list[Path], output: Path) -> dict[str, Any]:
    entries = []
    for path in files:
        if not path.is_file():
            raise FileNotFoundError(f"training triples file is absent: {path}")
        entries.append({"path": str(path.resolve()), "sha256": _sha256(path)})
    manifest = {"format": "ms-marco-triples-v1", "files": entries}
    output.parent.mkdir(parents=True, exist_ok=True)
    output.write_text(
        json.dumps(manifest, indent=2, sort_keys=True) + "\n", encoding="utf-8"
    )
    return manifest


@dataclass(frozen=True)
class Triple:
    query: str
    positive: str
    negative: str


def read_training_manifest(path: Path) -> tuple[list[Triple], list[dict[str, str]]]:
    if not path.is_file():
        raise FileNotFoundError(f"training manifest is absent: {path}")
    manifest = json.loads(path.read_text(encoding="utf-8"))
    if manifest.get("format") != "ms-marco-triples-v1":
        raise ValueError("training manifest format must be ms-marco-triples-v1")
    triples = []
    digests = []
    for entry in manifest.get("files", []):
        triples_path = Path(entry["path"])
        if not triples_path.is_file():
            raise FileNotFoundError(f"training triples file is absent: {triples_path}")
        actual = _sha256(triples_path)
        if actual != entry["sha256"]:
            raise ValueError(f"training file digest mismatch: {triples_path}")
        digests.append({"path": str(triples_path), "sha256": actual})
        with triples_path.open(encoding="utf-8") as source:
            for line_number, line in enumerate(source, 1):
                fields = line.rstrip("\n").split("\t")
                if len(fields) != 3:
                    raise ValueError(
                        f"{triples_path}:{line_number}: expected query, positive, negative"
                    )
                triples.append(Triple(*fields))
    if not triples:
        raise ValueError("training manifest contains no triples")
    return triples, digests


def _selected_layers(total: int, kind: str) -> list[int]:
    if kind == "q2":
        return [0, min(1, total - 1), max(total - 2, 0), total - 1]
    if kind == "q3":
        return [0, total - 1]
    raise ValueError("layer selection applies only to q2 and q3")


def _student_parameters(teacher: MlxBertEncoder, layers: list[int]) -> dict[str, Any]:
    prefixes = [teacher.base + "embeddings."] + [
        teacher.base + f"encoder.layer.{layer}." for layer in sorted(set(layers))
    ]
    return {
        name: value
        for name, value in teacher.mx_weights.items()
        if any(name.startswith(prefix) for prefix in prefixes)
    }


def _q4_forward_mx(mx, table, input_ids: np.ndarray, attention: np.ndarray):
    rows = []
    for ids, mask in zip(input_ids, attention, strict=True):
        retained = [
            int(token) for token, keep in zip(ids, mask, strict=True) if keep > 0.0
        ]
        canonical = mx.array(np.asarray(sorted(retained), dtype=np.int64))
        pooled = mx.mean(mx.take(table, canonical, axis=0), axis=0)
        norm = mx.sqrt(mx.sum(pooled * pooled))
        rows.append(pooled / mx.maximum(norm, np.finfo(np.float32).tiny))
    return mx.stack(rows)


def _padded_token_rows(
    teacher: NumpyBertEncoder, token_rows: list[list[int]]
) -> tuple[np.ndarray, np.ndarray]:
    width = max(len(row) for row in token_rows)
    ids = np.full((len(token_rows), width), teacher.pad_token_id, dtype=np.int64)
    mask = np.zeros((len(token_rows), width), dtype=np.float32)
    for index, row in enumerate(token_rows):
        ids[index, : len(row)] = row
        mask[index, : len(row)] = 1.0
    return ids, mask


def train(
    kind: str,
    teacher_dir: Path,
    manifest_path: Path,
    output: Path,
    *,
    seed: int = SEED,
    batch_size: int = DEFAULT_BATCH,
    learning_rate: float = DEFAULT_LR,
    warmup_steps: int = DEFAULT_WARMUP,
    temperature: float = DEFAULT_TEMPERATURE,
    max_steps: int | None = None,
    max_wall_hours: float = 12.0,
) -> dict[str, Any]:
    """Train Q2, Q3, or Q4; the frozen teacher supplies every target/doc vector."""
    if kind not in {"q2", "q3", "q4"}:
        raise ValueError("training kind must be q2, q3, or q4")
    import mlx.core as mx

    mx.random.seed(seed)
    np.random.seed(seed)
    teacher = MlxBertEncoder(teacher_dir)
    triples, data_digests = read_training_manifest(manifest_path)
    layer_indices = _selected_layers(teacher.layers, kind) if kind != "q4" else []
    if kind == "q4":
        parameters = {"table": mx.array(build_q4_table(teacher))}
    else:
        parameters = _student_parameters(teacher, layer_indices)
    first_moment = {name: mx.zeros_like(value) for name, value in parameters.items()}
    second_moment = {name: mx.zeros_like(value) for name, value in parameters.items()}

    def loss_function(trainable, query_ids, query_mask, teacher_queries, documents):
        if kind == "q4":
            student = _q4_forward_mx(mx, trainable["table"], query_ids, query_mask)
        else:
            student = teacher.forward_mx(
                query_ids, query_mask, weights=trainable, layer_indices=layer_indices
            )
        alignment = mx.mean((student - teacher_queries) ** 2)
        logits = (student @ mx.transpose(documents)) / temperature
        positive_logits = mx.diag(logits[:, : student.shape[0]])
        contrastive = mx.mean(mx.logsumexp(logits, axis=1) - positive_logits)
        return alignment + contrastive

    value_and_grad = mx.value_and_grad(loss_function)
    order = np.random.default_rng(seed).permutation(len(triples))
    total_steps = math.ceil(len(triples) / batch_size)
    started = time.perf_counter()
    losses = []
    steps_completed = 0
    budget_cut = False
    for offset in range(0, len(order), batch_size):
        if max_steps is not None and steps_completed >= max_steps:
            budget_cut = steps_completed < total_steps
            break
        if time.perf_counter() - started >= max_wall_hours * 3600.0:
            budget_cut = True
            break
        batch = [triples[int(index)] for index in order[offset : offset + batch_size]]
        teacher_query_ids, teacher_query_mask = teacher.prepare_texts(
            [item.query for item in batch]
        )
        if kind == "q4":
            query_ids, query_mask = _padded_token_rows(
                teacher, [teacher.table_token_ids(item.query) for item in batch]
            )
        else:
            query_ids, query_mask = teacher_query_ids, teacher_query_mask
        document_rows = [
            teacher.document_token_ids(text)
            for text in [item.positive for item in batch]
            + [item.negative for item in batch]
        ]
        doc_ids, doc_mask = _padded_token_rows(
            teacher,
            document_rows,
        )
        teacher_query = mx.stop_gradient(
            teacher.forward_mx(teacher_query_ids, teacher_query_mask)
        )
        documents = mx.stop_gradient(teacher.forward_mx(doc_ids, doc_mask))
        loss, gradients = value_and_grad(
            parameters, query_ids, query_mask, teacher_query, documents
        )
        steps_completed += 1
        scheduled_lr = learning_rate * min(1.0, steps_completed / max(warmup_steps, 1))
        beta1 = 0.9
        beta2 = 0.999
        for name in parameters:
            first_moment[name] = (
                beta1 * first_moment[name] + (1.0 - beta1) * gradients[name]
            )
            second_moment[name] = beta2 * second_moment[name] + (1.0 - beta2) * (
                gradients[name] * gradients[name]
            )
            corrected_first = first_moment[name] / (1.0 - beta1**steps_completed)
            corrected_second = second_moment[name] / (1.0 - beta2**steps_completed)
            parameters[name] = parameters[name] - scheduled_lr * corrected_first / (
                mx.sqrt(corrected_second) + 1.0e-8
            )
        mx.eval(loss, parameters, first_moment, second_moment)
        losses.append(float(loss.item()))
    wall_seconds = time.perf_counter() - started
    if steps_completed < total_steps:
        budget_cut = True
    converged = (
        None
        if budget_cut
        else bool(losses and np.isfinite(losses[-1]) and losses[-1] <= losses[0])
    )
    parameter_elements = sum(int(value.size) for value in parameters.values())
    report = {
        "cell": kind.upper(),
        "data_digests": data_digests,
        "seed": float(seed),
        "steps_completed": float(steps_completed),
        "wall_seconds": float(wall_seconds),
        "converged": converged,
        "lr": float(learning_rate),
        "batch": float(batch_size),
        "warmup": float(warmup_steps),
        "temperature": float(temperature),
        "mlx_version": str(mx.__version__),
        "max_steps": None if max_steps is None else float(max_steps),
        "max_wall_hours": float(max_wall_hours),
        "initial_loss": float(losses[0]) if losses else None,
        "final_loss": float(losses[-1]) if losses else None,
        "table_int8_bytes": int8_table_size_bytes(np.asarray(parameters["table"]))
        if kind == "q4"
        else None,
        "weights_fp16_bytes": float(parameter_elements * 2) if kind != "q4" else None,
        "weights_int8_bytes": float(parameter_elements) if kind != "q4" else None,
        "notes": ["converged: null because the configured budget cut the epoch"]
        if budget_cut
        else [],
    }
    output.mkdir(parents=True, exist_ok=True)
    arrays = {name: np.asarray(value) for name, value in parameters.items()}
    np.savez(output / "tower.npz", **arrays)
    (output / "tower.json").write_text(
        json.dumps(
            {**report, "kind": kind, "layer_indices": layer_indices},
            indent=2,
            sort_keys=True,
        )
        + "\n",
        encoding="utf-8",
    )
    return report


def _read_matrix(path: Path, rows: int, dims: int) -> np.ndarray:
    matrix = np.fromfile(path, dtype="<f4")
    if matrix.size != rows * dims:
        raise ValueError(f"{path}: vector byte count does not match ids and dimensions")
    return matrix.reshape(rows, dims)


def evaluate_tower(
    teacher_dir: Path,
    tower: str,
    beir_dir: Path,
    vectors_dir: Path,
    output_vectors: Path | None = None,
) -> dict[str, float]:
    corpus = load_beir(beir_dir)
    meta = json.loads((vectors_dir / "meta.json").read_text(encoding="utf-8"))
    corpus_ids = (
        (vectors_dir / "corpus_ids.txt").read_text(encoding="utf-8").splitlines()
    )
    document_vectors = _read_matrix(
        vectors_dir / "corpus_vectors.f32", len(corpus_ids), int(meta["dims"])
    )
    teacher = MlxBertEncoder(teacher_dir)
    texts = [query.text for query in corpus.queries]
    if tower == "q1":
        query_vectors = teacher.encode_texts(texts)
    else:
        tower_path = Path(tower)
        tower_meta = json.loads((tower_path / "tower.json").read_text(encoding="utf-8"))
        archive = np.load(tower_path / "tower.npz", allow_pickle=False)
        if tower_meta["kind"] == "q4":
            query_vectors = encode_q4(
                archive["table"], [teacher.table_token_ids(text) for text in texts]
            )
        else:
            weights = {name: teacher.mx.array(archive[name]) for name in archive.files}
            ids, mask = teacher.prepare_texts(texts)
            output = teacher.forward_mx(
                ids, mask, weights=weights, layer_indices=tower_meta["layer_indices"]
            )
            teacher.mx.eval(output)
            query_vectors = np.asarray(output)
    run = dense_retrieval(
        query_vectors, document_vectors, corpus.query_ids, corpus_ids, k=10
    )
    if output_vectors is not None:
        write_ragbench_vectors(
            output_vectors,
            document_vectors,
            query_vectors,
            corpus_ids,
            corpus.query_ids,
        )
    return {"dense_ndcg10": mean_ndcg_at_k(run, corpus.qrels, 10)}


def main() -> None:
    parser = argparse.ArgumentParser(description=__doc__)
    subparsers = parser.add_subparsers(dest="command", required=True)
    manifest = subparsers.add_parser("manifest")
    manifest.add_argument("--triples", required=True, nargs="+", type=Path)
    manifest.add_argument("--out", required=True, type=Path)
    train_parser = subparsers.add_parser("train")
    train_parser.add_argument("--cell", required=True, choices=("q2", "q3", "q4"))
    train_parser.add_argument("--teacher", required=True, type=Path)
    train_parser.add_argument("--data", required=True, type=Path)
    train_parser.add_argument("--out", required=True, type=Path)
    train_parser.add_argument("--seed", type=int, default=SEED)
    train_parser.add_argument("--batch", type=int, default=DEFAULT_BATCH)
    train_parser.add_argument("--lr", type=float, default=DEFAULT_LR)
    train_parser.add_argument("--warmup", type=int, default=DEFAULT_WARMUP)
    train_parser.add_argument("--temperature", type=float, default=DEFAULT_TEMPERATURE)
    train_parser.add_argument("--max-steps", type=int)
    train_parser.add_argument("--max-wall-hours", type=float, default=12.0)
    evaluate = subparsers.add_parser("eval")
    evaluate.add_argument("--teacher", required=True, type=Path)
    evaluate.add_argument("--tower", required=True)
    evaluate.add_argument("--beir", required=True, type=Path)
    evaluate.add_argument("--doc-vectors", required=True, type=Path)
    evaluate.add_argument("--out-vectors", type=Path)
    evaluate.add_argument("--out", required=True, type=Path)
    args = parser.parse_args()
    if args.command == "manifest":
        print(
            json.dumps(write_training_manifest(args.triples, args.out), sort_keys=True)
        )
    elif args.command == "train":
        report = train(
            args.cell,
            args.teacher,
            args.data,
            args.out,
            seed=args.seed,
            batch_size=args.batch,
            learning_rate=args.lr,
            warmup_steps=args.warmup,
            temperature=args.temperature,
            max_steps=args.max_steps,
            max_wall_hours=args.max_wall_hours,
        )
        print(json.dumps(report, sort_keys=True))
    else:
        report = evaluate_tower(
            args.teacher,
            args.tower,
            args.beir,
            args.doc_vectors,
            args.out_vectors,
        )
        args.out.parent.mkdir(parents=True, exist_ok=True)
        args.out.write_text(
            json.dumps(report, indent=2, sort_keys=True) + "\n", encoding="utf-8"
        )


if __name__ == "__main__":
    main()
