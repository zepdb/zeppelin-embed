from __future__ import annotations

import json
import struct
from pathlib import Path

import numpy as np
from safetensors.numpy import load_file, save_file
from tokenizers import Tokenizer
from tokenizers.models import WordLevel
from tokenizers.pre_tokenizers import Whitespace

SEED = 20260903


def rewrite_safetensors_as_bfloat16(path: Path) -> None:
    tensors = load_file(path)
    header: dict[str, object] = {"__metadata__": {"format": "pt"}}
    payload = bytearray()
    for name, tensor in sorted(tensors.items()):
        float32 = tensor.astype(np.float32)
        words = (float32.view(np.uint32) >> 16).astype("<u2")
        start = len(payload)
        payload.extend(words.tobytes())
        header[name] = {
            "dtype": "BF16",
            "shape": list(tensor.shape),
            "data_offsets": [start, len(payload)],
        }
    encoded = json.dumps(header, separators=(",", ":")).encode("utf-8")
    encoded += b" " * (-len(encoded) % 8)
    path.write_bytes(struct.pack("<Q", len(encoded)) + encoded + payload)


def write_tiny_bert(root: Path) -> Path:
    model = root / "source"
    model.mkdir(parents=True)
    vocabulary = {"[PAD]": 0, "[UNK]": 1}
    vocabulary.update({f"t{index}": index + 2 for index in range(62)})
    tokenizer = Tokenizer(WordLevel(vocabulary, unk_token="[UNK]"))
    tokenizer.pre_tokenizer = Whitespace()
    tokenizer.save(str(model / "tokenizer.json"))

    config = {
        "architectures": ["BertModel"],
        "model_type": "bert",
        "vocab_size": 64,
        "hidden_size": 16,
        "num_hidden_layers": 2,
        "num_attention_heads": 2,
        "intermediate_size": 32,
        "max_position_embeddings": 64,
        "type_vocab_size": 2,
        "hidden_act": "gelu",
        "layer_norm_eps": 1e-5,
        "pad_token_id": 0,
        "pooling": "mean",
        "normalize": True,
        "prompt_prefix": "",
        "max_tokens": 64,
        "model_version": "synthetic-v1",
    }
    (model / "config.json").write_text(json.dumps(config), encoding="utf-8")

    rng = np.random.default_rng(SEED)
    weights: dict[str, np.ndarray] = {}
    prefix = "bert."
    weights[prefix + "embeddings.word_embeddings.weight"] = rng.normal(
        0.0, 0.5, (64, 16)
    ).astype(np.float32)
    weights[prefix + "embeddings.position_embeddings.weight"] = rng.normal(
        0.0, 0.03, (64, 16)
    ).astype(np.float32)
    weights[prefix + "embeddings.token_type_embeddings.weight"] = np.zeros(
        (2, 16), dtype=np.float32
    )
    weights[prefix + "embeddings.LayerNorm.weight"] = np.ones(16, dtype=np.float32)
    weights[prefix + "embeddings.LayerNorm.bias"] = np.zeros(16, dtype=np.float32)
    for layer in range(2):
        base = prefix + f"encoder.layer.{layer}."
        for name in ("query", "key", "value"):
            weights[base + f"attention.self.{name}.weight"] = rng.normal(
                0.0, 0.08, (16, 16)
            ).astype(np.float32)
            weights[base + f"attention.self.{name}.bias"] = rng.normal(
                0.0, 0.01, 16
            ).astype(np.float32)
        weights[base + "attention.output.dense.weight"] = rng.normal(
            0.0, 0.08, (16, 16)
        ).astype(np.float32)
        weights[base + "attention.output.dense.bias"] = np.zeros(16, dtype=np.float32)
        weights[base + "attention.output.LayerNorm.weight"] = np.ones(
            16, dtype=np.float32
        )
        weights[base + "attention.output.LayerNorm.bias"] = np.zeros(
            16, dtype=np.float32
        )
        weights[base + "intermediate.dense.weight"] = rng.normal(
            0.0, 0.08, (32, 16)
        ).astype(np.float32)
        weights[base + "intermediate.dense.bias"] = np.zeros(32, dtype=np.float32)
        weights[base + "output.dense.weight"] = rng.normal(0.0, 0.08, (16, 32)).astype(
            np.float32
        )
        weights[base + "output.dense.bias"] = np.zeros(16, dtype=np.float32)
        weights[base + "output.LayerNorm.weight"] = np.ones(16, dtype=np.float32)
        weights[base + "output.LayerNorm.bias"] = np.zeros(16, dtype=np.float32)
    save_file(weights, model / "model.safetensors")
    return model


def write_tiny_modernbert(root: Path) -> Path:
    model = root / "modernbert-source"
    model.mkdir(parents=True)
    vocabulary = {"[PAD]": 0, "[UNK]": 1}
    vocabulary.update({f"t{index}": index + 2 for index in range(62)})
    tokenizer = Tokenizer(WordLevel(vocabulary, unk_token="[UNK]"))
    tokenizer.pre_tokenizer = Whitespace()
    tokenizer.save(str(model / "tokenizer.json"))

    config = {
        "architectures": ["ModernBertModel"],
        "model_type": "modernbert",
        "vocab_size": 64,
        "hidden_size": 8,
        "num_hidden_layers": 2,
        "num_attention_heads": 2,
        "intermediate_size": 16,
        "hidden_activation": "silu",
        "norm_eps": 1e-5,
        "norm_bias": False,
        "attention_bias": False,
        "mlp_bias": False,
        "max_position_embeddings": 64,
        "layer_types": ["full_attention", "sliding_attention"],
        "rope_parameters": {
            "full_attention": {"rope_type": "default", "rope_theta": 160000.0},
            "sliding_attention": {"rope_type": "default", "rope_theta": 10000.0},
        },
        "local_attention": 4,
        "pad_token_id": 0,
        "pooling": "cls",
        "normalize": True,
        "prompt_prefix": "",
        "max_tokens": 64,
    }
    (model / "config.json").write_text(json.dumps(config), encoding="utf-8")

    rng = np.random.default_rng(SEED + 1)
    weights: dict[str, np.ndarray] = {}
    prefix = "model."
    weights[prefix + "embeddings.tok_embeddings.weight"] = rng.normal(
        0.0, 0.5, (64, 8)
    ).astype(np.float32)
    weights[prefix + "embeddings.norm.weight"] = rng.normal(
        1.0, 0.02, 8
    ).astype(np.float32)
    for layer in range(2):
        base = prefix + f"layers.{layer}."
        if layer:
            weights[base + "attn_norm.weight"] = rng.normal(
                1.0, 0.02, 8
            ).astype(np.float32)
        weights[base + "attn.Wqkv.weight"] = rng.normal(
            0.0, 0.08, (24, 8)
        ).astype(np.float32)
        weights[base + "attn.Wo.weight"] = rng.normal(
            0.0, 0.08, (8, 8)
        ).astype(np.float32)
        weights[base + "mlp_norm.weight"] = rng.normal(
            1.0, 0.02, 8
        ).astype(np.float32)
        weights[base + "mlp.Wi.weight"] = rng.normal(
            0.0, 0.08, (32, 8)
        ).astype(np.float32)
        weights[base + "mlp.Wo.weight"] = rng.normal(
            0.0, 0.08, (8, 16)
        ).astype(np.float32)
    weights[prefix + "final_norm.weight"] = rng.normal(
        1.0, 0.02, 8
    ).astype(np.float32)
    save_file(weights, model / "model.safetensors")
    return model


def write_beir(root: Path) -> Path:
    beir = root / "beir" / "synthetic"
    (beir / "qrels").mkdir(parents=True)
    with (beir / "corpus.jsonl").open("w", encoding="utf-8") as output:
        for index in range(20):
            output.write(
                json.dumps({"_id": f"d{index}", "title": "", "text": f"t{index}"})
                + "\n"
            )
    with (beir / "queries.jsonl").open("w", encoding="utf-8") as output:
        for index in range(20):
            output.write(json.dumps({"_id": f"q{index}", "text": f"t{index}"}) + "\n")
    with (beir / "qrels" / "test.tsv").open("w", encoding="utf-8") as output:
        output.write("query-id\tcorpus-id\tscore\n")
        for index in range(20):
            output.write(f"q{index}\td{index}\t1\n")
    return beir
