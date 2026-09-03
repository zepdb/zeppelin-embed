"""Create one complete, null-disciplined JSON result for one cell."""

from __future__ import annotations

import argparse
import hashlib
import json
import subprocess
from pathlib import Path
from typing import Any

MEASUREMENT_FIELDS = (
    "cold_first_ms",
    "latency_16_p50_ms",
    "latency_16_p95_ms",
    "latency_16_p99_ms",
    "latency_16_cycles",
    "latency_32_p50_ms",
    "latency_32_p95_ms",
    "latency_32_p99_ms",
    "latency_32_cycles",
    "throughput_32_padded_tokens_s",
    "throughput_32_real_tokens_s",
    "throughput_128_padded_tokens_s",
    "throughput_128_real_tokens_s",
    "throughput_512_padded_tokens_s",
    "throughput_512_real_tokens_s",
    "dense_ndcg10",
    "hybrid_ndcg10",
    "retention_plain",
    "retention_multihop_entity",
    "retention_multilingual",
    "rss_bytes",
    "table_int8_bytes",
)


def _uptime() -> str | None:
    try:
        completed = subprocess.run(
            ["uptime"], check=True, capture_output=True, text=True
        )
    except (OSError, subprocess.CalledProcessError):
        return None
    return completed.stdout.strip()


def _mlx_version() -> str | None:
    try:
        import mlx.core  # type: ignore[import-not-found]
    except ImportError:
        return None
    return str(mlx.core.__version__)


def _git_commit() -> str | None:
    try:
        completed = subprocess.run(
            ["git", "rev-parse", "HEAD"],
            check=True,
            capture_output=True,
            text=True,
        )
    except (OSError, subprocess.CalledProcessError):
        return None
    return completed.stdout.strip() or None


def _weights_digest(model_dir: Path | None) -> str | None:
    if model_dir is None:
        return None
    meta_path = model_dir / "meta.json"
    try:
        meta = json.loads(meta_path.read_text(encoding="utf-8"))
    except (OSError, json.JSONDecodeError):
        return None
    digest = meta.get("weights_digest") or meta.get("source_safetensors_sha256")
    return digest if isinstance(digest, str) else None


def file_sha256(path: Path) -> str:
    digest = hashlib.sha256()
    with path.open("rb") as source:
        for chunk in iter(lambda: source.read(1024 * 1024), b""):
            digest.update(chunk)
    return digest.hexdigest()


def new_result(cell: str, model_dir: Path | None = None) -> dict[str, Any]:
    result: dict[str, Any] = {
        "schema_version": "embed-harness-cell-v1",
        "cell": cell,
        "declared_fields": list(MEASUREMENT_FIELDS),
        "notes": [f"{field}: not recorded" for field in MEASUREMENT_FIELDS],
        "uptime_before": _uptime(),
        "uptime_after": None,
        "mlx_version": _mlx_version(),
        "weights_sha256": _weights_digest(model_dir),
        "harness_git_commit": _git_commit(),
        "void": False,
        "taint": [],
    }
    result.update({field: None for field in MEASUREMENT_FIELDS})
    for field in ("uptime_after", "mlx_version", "weights_sha256"):
        if result[field] is None:
            result["notes"].append(f"{field}: unavailable")
    if result["harness_git_commit"] is None:
        result["notes"].append("harness_git_commit: unavailable")
    return result


def _remove_field_notes(result: dict[str, Any], field: str) -> None:
    prefix = f"{field}:"
    result["notes"] = [note for note in result["notes"] if not note.startswith(prefix)]


def record_number(result: dict[str, Any], field: str, value: float) -> None:
    if field not in MEASUREMENT_FIELDS:
        raise KeyError(f"undeclared measurement field: {field}")
    if isinstance(value, bool) or not isinstance(value, float):
        raise TypeError(f"{field} must be an explicit float")
    result[field] = value
    _remove_field_notes(result, field)


def record_skipped(result: dict[str, Any], field: str, reason: str) -> None:
    if field not in MEASUREMENT_FIELDS:
        raise KeyError(f"undeclared measurement field: {field}")
    result[field] = None
    _remove_field_notes(result, field)
    result["notes"].append(f"{field}: {reason}")


def finish_result(result: dict[str, Any]) -> None:
    result["uptime_after"] = _uptime()
    _remove_field_notes(result, "uptime_after")
    if result["uptime_after"] is None:
        result["notes"].append("uptime_after: unavailable")


def validate_result(result: dict[str, Any]) -> None:
    for field in result["declared_fields"]:
        if field not in result:
            raise ValueError(f"missing declared field: {field}")
        value = result[field]
        if value is None:
            if not any(note.startswith(f"{field}:") for note in result["notes"]):
                raise ValueError(f"null field lacks a note: {field}")
        elif isinstance(value, bool) or not isinstance(value, float):
            raise TypeError(f"measurement {field} is not an explicit float")


def write_result(path: Path, result: dict[str, Any]) -> None:
    finish_result(result)
    validate_result(result)
    path.parent.mkdir(parents=True, exist_ok=True)
    path.write_text(
        json.dumps(result, indent=2, sort_keys=True) + "\n", encoding="utf-8"
    )


def main() -> None:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--cell", required=True)
    parser.add_argument("--model", type=Path)
    parser.add_argument("--out", required=True, type=Path)
    parser.add_argument("--taint", action="append", default=[])
    parser.add_argument("--skip", action="append", default=[], metavar="FIELD=REASON")
    parser.add_argument("--number", action="append", default=[], metavar="FIELD=FLOAT")
    args = parser.parse_args()
    result = new_result(args.cell, args.model)
    result["taint"] = list(args.taint)
    for item in args.number:
        field, separator, value = item.partition("=")
        if not separator:
            parser.error("--number must be FIELD=FLOAT")
        record_number(result, field, float(value))
    for item in args.skip:
        field, separator, reason = item.partition("=")
        if not separator or not reason:
            parser.error("--skip must be FIELD=REASON")
        record_skipped(result, field, reason)
    write_result(args.out, result)


if __name__ == "__main__":
    main()
