"""Convert a local Hugging Face safetensors BERT-class model to NPZ."""

from __future__ import annotations

import argparse
import hashlib
import json
import platform
import shutil
from pathlib import Path
from typing import Any

import numpy as np
from safetensors.numpy import load_file

BERT_ARCHITECTURES = {"BertModel", "XLMRobertaModel", "RobertaModel"}


def sha256_file(path: Path) -> str:
    digest = hashlib.sha256()
    with path.open("rb") as source:
        for chunk in iter(lambda: source.read(1024 * 1024), b""):
            digest.update(chunk)
    return digest.hexdigest()


def _architecture(config: dict[str, Any]) -> str:
    architectures = config.get("architectures")
    if not isinstance(architectures, list) or not architectures:
        raise ValueError("config.json must name at least one architecture")
    architecture = str(architectures[0])
    lowered = architecture.lower()
    if architecture in BERT_ARCHITECTURES:
        return architecture
    if "modernbert" in lowered:
        raise NotImplementedError("ModernBERT: converter not implemented")
    if "gemma3" in lowered or "gemma-3" in lowered:
        raise NotImplementedError("Gemma-3: converter not implemented")
    if "qwen3" in lowered:
        raise NotImplementedError("Qwen3: converter not implemented")
    raise ValueError(f"unsupported architecture: {architecture}")


def _required_source_files(source: Path) -> tuple[Path, Path]:
    if not source.is_dir():
        raise FileNotFoundError(f"local model directory does not exist: {source}")
    config = source / "config.json"
    weights = source / "model.safetensors"
    if not config.is_file():
        raise FileNotFoundError(f"required local model file is absent: {config}")
    if not weights.is_file():
        raise FileNotFoundError(f"required local model file is absent: {weights}")
    return config, weights


def convert_model(
    source: Path,
    output: Path,
    *,
    pooling: str | None = None,
    prompt_prefix: str | None = None,
    document_prefix: str | None = None,
    normalize: bool | None = None,
    max_tokens: int | None = None,
) -> dict[str, Any]:
    """Convert one local model without network access."""
    config_path, safetensors_path = _required_source_files(source)
    config = json.loads(config_path.read_text(encoding="utf-8"))
    architecture = _architecture(config)
    tensors = load_file(safetensors_path)
    if not tensors:
        raise ValueError("model.safetensors contains no tensors")
    numeric_tensors: dict[str, np.ndarray] = {}
    for name, tensor in tensors.items():
        if tensor.dtype.kind == "V" and tensor.dtype.itemsize == 2:
            # safetensors exposes BF16 as two opaque bytes when NumPy has no
            # native bfloat16 dtype. Expand it exactly into float32 bits.
            words = tensor.view(np.uint16).astype(np.uint32)
            numeric_tensors[name] = (words << 16).view(np.float32)
        else:
            numeric_tensors[name] = tensor

    hidden_size = int(config["hidden_size"])
    selected_pooling = pooling or str(config.get("pooling", "mean"))
    if selected_pooling not in {"mean", "cls", "last-token"}:
        raise ValueError("pooling must be mean, cls, or last-token")
    selected_prefix = prompt_prefix
    if selected_prefix is None:
        selected_prefix = str(config.get("prompt_prefix", ""))
    selected_document_prefix = document_prefix
    if selected_document_prefix is None:
        selected_document_prefix = str(config.get("document_prefix", selected_prefix))
    selected_normalize = (
        bool(config.get("normalize", True)) if normalize is None else normalize
    )
    selected_max_tokens = max_tokens or int(
        config.get("max_tokens", config.get("max_position_embeddings", 512))
    )
    digest = sha256_file(safetensors_path)
    meta: dict[str, Any] = {
        "source_safetensors_sha256": digest,
        "architecture": architecture,
        "dims": hidden_size,
        "pooling": selected_pooling,
        "prefix_convention": {
            "query": selected_prefix,
            "document": selected_document_prefix,
        },
        "max_tokens": selected_max_tokens,
        "model_id": config.get("_name_or_path") or source.name,
        "model_version": config.get("model_version"),
        "weights_digest": digest,
        "normalization": "l2" if selected_normalize else "none",
        "prompt_prefix": selected_prefix,
        "document_prefix": selected_document_prefix,
        "runtime": "mlx",
        "compute_units": "gpu",
        "os_build": platform.platform(),
        "config": {
            key: config.get(key)
            for key in (
                "vocab_size",
                "hidden_size",
                "num_hidden_layers",
                "num_attention_heads",
                "intermediate_size",
                "hidden_act",
                "layer_norm_eps",
                "max_position_embeddings",
                "type_vocab_size",
                "pad_token_id",
            )
        },
    }
    output.mkdir(parents=True, exist_ok=True)
    np.savez(output / "weights.npz", **numeric_tensors)
    copied = 0
    for artifact in source.iterdir():
        if artifact.is_file() and (
            artifact.name.startswith("tokenizer.")
            or artifact.name in {"sentencepiece.bpe.model", "spiece.model"}
        ):
            shutil.copy2(artifact, output / artifact.name)
            copied += 1
    if copied == 0:
        raise FileNotFoundError(
            f"no tokenizer artifact (tokenizer.* or sentencepiece model) in {source}"
        )
    (output / "meta.json").write_text(
        json.dumps(meta, indent=2, sort_keys=True) + "\n", encoding="utf-8"
    )
    return meta


def main() -> None:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--source", required=True, type=Path)
    parser.add_argument("--out", required=True, type=Path)
    parser.add_argument("--pooling", choices=("mean", "cls", "last-token"))
    parser.add_argument("--prompt-prefix")
    parser.add_argument("--document-prefix")
    parser.add_argument("--max-tokens", type=int)
    normalization = parser.add_mutually_exclusive_group()
    normalization.add_argument("--normalize", dest="normalize", action="store_true")
    normalization.add_argument("--no-normalize", dest="normalize", action="store_false")
    parser.set_defaults(normalize=None)
    args = parser.parse_args()
    try:
        meta = convert_model(
            args.source,
            args.out,
            pooling=args.pooling,
            prompt_prefix=args.prompt_prefix,
            document_prefix=args.document_prefix,
            normalize=args.normalize,
            max_tokens=args.max_tokens,
        )
    except (FileNotFoundError, ValueError, NotImplementedError) as error:
        parser.exit(2, f"embed-harness convert: {error}\n")
    print(json.dumps(meta, sort_keys=True))


if __name__ == "__main__":
    main()
