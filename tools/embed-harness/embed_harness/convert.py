"""Convert a local Hugging Face safetensors encoder model to NPZ."""

from __future__ import annotations

import argparse
import hashlib
import json
import platform
import shutil
import struct
from pathlib import Path
from typing import Any

import numpy as np
from tokenizers import Tokenizer

BERT_ARCHITECTURES = {"BertModel", "XLMRobertaModel", "RobertaModel"}
MODERNBERT_ARCHITECTURES = {"ModernBertModel"}


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
    if architecture in MODERNBERT_ARCHITECTURES:
        return architecture
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


def _load_safetensors(path: Path) -> dict[str, np.ndarray]:
    """Load standard numeric tensors, expanding BF16 without requiring PyTorch."""
    file_size = path.stat().st_size
    with path.open("rb") as source:
        encoded_length = source.read(8)
        if len(encoded_length) != 8:
            raise ValueError(f"invalid safetensors header in {path}")
        header_length = struct.unpack("<Q", encoded_length)[0]
        if header_length > file_size - 8:
            raise ValueError(f"invalid safetensors header length in {path}")
        header = json.loads(source.read(header_length).decode("utf-8"))
    if not isinstance(header, dict):
        raise ValueError(f"invalid safetensors metadata in {path}")

    dtypes = {
        "BOOL": np.dtype("?"),
        "U8": np.dtype("u1"),
        "I8": np.dtype("i1"),
        "U16": np.dtype("<u2"),
        "I16": np.dtype("<i2"),
        "U32": np.dtype("<u4"),
        "I32": np.dtype("<i4"),
        "U64": np.dtype("<u8"),
        "I64": np.dtype("<i8"),
        "F16": np.dtype("<f2"),
        "F32": np.dtype("<f4"),
        "F64": np.dtype("<f8"),
    }
    data_start = 8 + header_length
    tensors: dict[str, np.ndarray] = {}
    for name, descriptor in header.items():
        if name == "__metadata__":
            continue
        if not isinstance(descriptor, dict):
            raise ValueError(f"invalid descriptor for safetensors tensor {name}")
        dtype_name = str(descriptor.get("dtype"))
        shape = tuple(int(value) for value in descriptor.get("shape", []))
        offsets = descriptor.get("data_offsets")
        if not isinstance(offsets, list) or len(offsets) != 2:
            raise ValueError(f"invalid offsets for safetensors tensor {name}")
        start, end = (int(value) for value in offsets)
        count = int(np.prod(shape, dtype=np.int64))
        if start < 0 or end < start or data_start + end > file_size:
            raise ValueError(f"out-of-range safetensors tensor {name}")
        if dtype_name == "BF16":
            if end - start != count * 2:
                raise ValueError(f"invalid BF16 byte length for tensor {name}")
            raw = np.memmap(
                path,
                mode="r",
                dtype="<u2",
                offset=data_start + start,
                shape=(count,),
            )
            words = np.asarray(raw).astype(np.uint32)
            tensors[name] = (words << 16).view(np.float32).reshape(shape)
            continue
        try:
            dtype = dtypes[dtype_name]
        except KeyError as error:
            raise ValueError(
                f"unsupported safetensors dtype {dtype_name} for tensor {name}"
            ) from error
        if end - start != count * dtype.itemsize:
            raise ValueError(f"invalid byte length for safetensors tensor {name}")
        raw = np.memmap(
            path,
            mode="r",
            dtype=dtype,
            offset=data_start + start,
            shape=(count,),
        )
        tensors[name] = np.asarray(raw).reshape(shape).copy()
    return tensors


def convert_model(
    source: Path,
    output: Path,
    *,
    model_id: str,
    model_version: str,
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
    tensors = _load_safetensors(safetensors_path)
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
    runtime_config = {
        key: config.get(key)
        for key in (
            "vocab_size",
            "hidden_size",
            "num_hidden_layers",
            "num_attention_heads",
            "intermediate_size",
            "hidden_act",
            "hidden_activation",
            "layer_norm_eps",
            "norm_eps",
            "norm_bias",
            "attention_bias",
            "mlp_bias",
            "max_position_embeddings",
            "type_vocab_size",
            "pad_token_id",
            "layer_types",
            "global_attn_every_n_layers",
            "rope_parameters",
            "global_rope_theta",
            "local_rope_theta",
            "local_attention",
        )
    }
    tokenizer_config_path = source / "tokenizer_config.json"
    tokenizer_path = source / "tokenizer.json"
    if tokenizer_config_path.is_file() and tokenizer_path.is_file():
        tokenizer_config = json.loads(
            tokenizer_config_path.read_text(encoding="utf-8")
        )
        pad_token = tokenizer_config.get("pad_token")
        if isinstance(pad_token, dict):
            pad_token = pad_token.get("content")
        if isinstance(pad_token, str):
            tokenizer_pad_id = Tokenizer.from_file(str(tokenizer_path)).token_to_id(
                pad_token
            )
            if tokenizer_pad_id is None:
                raise ValueError(
                    f"tokenizer pad token {pad_token!r} has no vocabulary id"
                )
            runtime_config["pad_token_id"] = tokenizer_pad_id
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
        "model_id": model_id,
        "model_version": model_version,
        "weights_digest": digest,
        "normalization": "l2" if selected_normalize else "none",
        "prompt_prefix": selected_prefix,
        "document_prefix": selected_document_prefix,
        "runtime": "mlx",
        "compute_units": "gpu",
        "os_build": platform.platform(),
        "config": runtime_config,
    }
    output.mkdir(parents=True, exist_ok=True)
    np.savez(output / "weights.npz", **numeric_tensors)
    copied = 0
    for artifact in source.iterdir():
        if artifact.is_file() and (
            artifact.name.startswith("tokenizer.")
            or artifact.name
            in {
                "added_tokens.json",
                "sentencepiece.bpe.model",
                "special_tokens_map.json",
                "spiece.model",
                "tokenizer_config.json",
            }
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
    parser.add_argument("--model-id", required=True)
    parser.add_argument("--model-version", required=True)
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
            model_id=args.model_id,
            model_version=args.model_version,
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
