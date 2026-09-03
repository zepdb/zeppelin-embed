"""BERT-class NumPy/MLX encoding, latency, and throughput utilities."""

from __future__ import annotations

import argparse
import hashlib
import json
import math
import os
import time
from collections import Counter
from collections.abc import Callable
from pathlib import Path
from typing import Any

for variable in (
    "OMP_NUM_THREADS",
    "OPENBLAS_NUM_THREADS",
    "MKL_NUM_THREADS",
    "VECLIB_MAXIMUM_THREADS",
    "NUMEXPR_NUM_THREADS",
):
    os.environ.setdefault(variable, "1")

import numpy as np
from tokenizers import Tokenizer


def bracket(
    before_fn: Callable[[], float],
    after_fn: Callable[[], float],
    reference_p50_ms: float,
    tolerance: float = 0.05,
    cell_fn: Callable[[], Any] | None = None,
) -> dict[str, Any]:
    """Run the control before/after and void the enclosed cell on drift."""
    if reference_p50_ms <= 0.0:
        raise ValueError("control reference_p50_ms must be positive")
    before = float(before_fn())
    cell_result = cell_fn() if cell_fn is not None else None
    after = float(after_fn())
    low = reference_p50_ms * (1.0 - tolerance)
    high = reference_p50_ms * (1.0 + tolerance)
    notes = []
    for label, value in (("before", before), ("after", after)):
        if not low <= value <= high:
            notes.append(
                f"{label} control p50 {value:.6f} ms is outside "
                f"+/-{tolerance * 100.0:.1f}% of {reference_p50_ms:.6f} ms"
            )
    return {
        "before_p50_ms": before,
        "after_p50_ms": after,
        "reference_p50_ms": float(reference_p50_ms),
        "tolerance": float(tolerance),
        "void": bool(notes),
        "notes": notes,
        "cell_result": cell_result,
    }


def run_bracketed_cell(
    control_path: Path,
    before_fn: Callable[[], float],
    cell_fn: Callable[[], Any],
    after_fn: Callable[[], float],
) -> dict[str, Any]:
    control = json.loads(control_path.read_text(encoding="utf-8"))
    reference = control.get("reference_p50_ms")
    if not isinstance(reference, (float, int)) or isinstance(reference, bool):
        raise TypeError("control.json lacks a numeric reference_p50_ms")
    return bracket(
        before_fn,
        after_fn,
        float(reference),
        tolerance=0.05,
        cell_fn=cell_fn,
    )


def _gelu_numpy(values: np.ndarray) -> np.ndarray:
    flat = values.astype(np.float64, copy=False).ravel()
    erf = np.fromiter(
        (math.erf(value / math.sqrt(2.0)) for value in flat), dtype=np.float64
    )
    return (0.5 * flat * (1.0 + erf)).reshape(values.shape).astype(np.float32)


def _layer_norm_numpy(
    values: np.ndarray, weight: np.ndarray, bias: np.ndarray, epsilon: float
) -> np.ndarray:
    mean = values.mean(axis=-1, keepdims=True)
    variance = ((values - mean) ** 2).mean(axis=-1, keepdims=True)
    return ((values - mean) / np.sqrt(variance + epsilon)) * weight + bias


def _softmax_numpy(values: np.ndarray) -> np.ndarray:
    shifted = values - values.max(axis=-1, keepdims=True)
    exponent = np.exp(shifted)
    return exponent / exponent.sum(axis=-1, keepdims=True)


def _silu_numpy(values: np.ndarray) -> np.ndarray:
    return values / (1.0 + np.exp(-values))


class NumpyBertEncoder:
    """Single-thread reference BERT/XLM-R forward over converted weights."""

    def __init__(self, model_dir: Path):
        self.model_dir = Path(model_dir)
        self.meta = json.loads(
            (self.model_dir / "meta.json").read_text(encoding="utf-8")
        )
        archive = np.load(self.model_dir / "weights.npz", allow_pickle=False)
        self.weights = {
            name: archive[name].astype(np.float32) for name in archive.files
        }
        self.tokenizer = Tokenizer.from_file(str(self.model_dir / "tokenizer.json"))
        self.config = self.meta["config"]
        self.prefix = str(self.meta["prompt_prefix"])
        self.document_prefix = str(self.meta.get("document_prefix", self.prefix))
        self.pooling = str(self.meta["pooling"])
        self.normalize = self.meta["normalization"] == "l2"
        self.pad_token_id = int(self.config.get("pad_token_id") or 0)
        self.hidden_size = int(self.config["hidden_size"])
        self.heads = int(self.config["num_attention_heads"])
        if self.hidden_size % self.heads:
            raise ValueError("hidden_size must be divisible by num_attention_heads")
        self.layers = int(self.config["num_hidden_layers"])
        self.architecture = str(self.meta["architecture"])
        word_suffix = (
            "embeddings.tok_embeddings.weight"
            if self.architecture == "ModernBertModel"
            else "embeddings.word_embeddings.weight"
        )
        matches = [name for name in self.weights if name.endswith(word_suffix)]
        if len(matches) != 1:
            raise ValueError(f"expected one {word_suffix} tensor, found {len(matches)}")
        self.base = matches[0][: -len(word_suffix)]

    @property
    def vocab_size(self) -> int:
        suffix = (
            "embeddings.tok_embeddings.weight"
            if self.architecture == "ModernBertModel"
            else "embeddings.word_embeddings.weight"
        )
        return int(self._weight(suffix).shape[0])

    @property
    def dims(self) -> int:
        return self.hidden_size

    def _weight(self, suffix: str) -> np.ndarray:
        name = self.base + suffix
        try:
            return self.weights[name]
        except KeyError as error:
            raise KeyError(f"converted model lacks tensor {name}") from error

    def _optional_weight(self, suffix: str) -> np.ndarray | None:
        return self.weights.get(self.base + suffix)

    def token_ids(self, text: str) -> list[int]:
        ids = self.tokenizer.encode(self.prefix + text, add_special_tokens=True).ids
        return ids or [self.pad_token_id]

    def document_token_ids(self, text: str) -> list[int]:
        ids = self.tokenizer.encode(
            self.document_prefix + text, add_special_tokens=True
        ).ids
        return ids or [self.pad_token_id]

    def table_token_ids(self, text: str) -> list[int]:
        """Content tokens only; every Q4 row already carries the prefix."""
        ids = self.tokenizer.encode(text, add_special_tokens=False).ids
        return ids or [self.pad_token_id]

    def wrap_special_tokens(self, raw_ids: list[int]) -> list[int]:
        probe_raw = self.tokenizer.encode(
            "embedding harness probe", add_special_tokens=False
        ).ids
        probe_full = self.tokenizer.encode(
            "embedding harness probe", add_special_tokens=True
        ).ids
        start = next(
            (
                index
                for index in range(len(probe_full) - len(probe_raw) + 1)
                if probe_full[index : index + len(probe_raw)] == probe_raw
            ),
            None,
        )
        if start is None:
            raise ValueError(
                "tokenizer special-token template is not a prefix/suffix wrapper"
            )
        return probe_full[:start] + raw_ids + probe_full[start + len(probe_raw) :]

    def prepare_texts(
        self, texts: list[str], exact_length: int | None = None
    ) -> tuple[np.ndarray, np.ndarray]:
        if not texts:
            raise ValueError("at least one text is required")
        rows = [self.token_ids(text) for text in texts]
        width = exact_length or min(
            max(len(row) for row in rows), int(self.meta["max_tokens"])
        )
        if width <= 0:
            raise ValueError("token length must be positive")
        input_ids = np.full((len(rows), width), self.pad_token_id, dtype=np.int64)
        attention = np.zeros((len(rows), width), dtype=np.float32)
        for index, row in enumerate(rows):
            retained = row[:width]
            input_ids[index, : len(retained)] = retained
            attention[index, : len(retained)] = 1.0
        return input_ids, attention

    def _position_ids(self, input_ids: np.ndarray, attention: np.ndarray) -> np.ndarray:
        if self.meta["architecture"] == "XLMRobertaModel":
            return (
                np.cumsum(attention.astype(np.int64), axis=1) * attention
                + self.pad_token_id
            ).astype(np.int64)
        return np.broadcast_to(
            np.arange(input_ids.shape[1], dtype=np.int64), input_ids.shape
        )

    def forward(self, input_ids: np.ndarray, attention: np.ndarray) -> np.ndarray:
        if self.architecture == "ModernBertModel":
            return self._forward_modernbert(input_ids, attention)
        position_ids = self._position_ids(input_ids, attention)
        token_types = np.zeros_like(input_ids)
        hidden = self._weight("embeddings.word_embeddings.weight")[input_ids]
        hidden = (
            hidden + self._weight("embeddings.position_embeddings.weight")[position_ids]
        )
        hidden = (
            hidden
            + self._weight("embeddings.token_type_embeddings.weight")[token_types]
        )
        epsilon = float(self.config.get("layer_norm_eps") or 1e-12)
        hidden = _layer_norm_numpy(
            hidden,
            self._weight("embeddings.LayerNorm.weight"),
            self._weight("embeddings.LayerNorm.bias"),
            epsilon,
        )
        head_size = self.hidden_size // self.heads
        attention_bias = (1.0 - attention[:, None, None, :]) * np.float32(-1.0e9)
        for layer in range(self.layers):
            base = f"encoder.layer.{layer}."
            projected = []
            for name in ("query", "key", "value"):
                projected.append(
                    hidden @ self._weight(base + f"attention.self.{name}.weight").T
                    + self._weight(base + f"attention.self.{name}.bias")
                )
            query, key, value = (
                item.reshape(
                    len(input_ids), input_ids.shape[1], self.heads, head_size
                ).transpose(0, 2, 1, 3)
                for item in projected
            )
            scores = (query @ key.transpose(0, 1, 3, 2)) / math.sqrt(float(head_size))
            probabilities = _softmax_numpy(scores + attention_bias)
            context = (
                (probabilities @ value).transpose(0, 2, 1, 3).reshape(hidden.shape)
            )
            attention_output = context @ self._weight(
                base + "attention.output.dense.weight"
            ).T + self._weight(base + "attention.output.dense.bias")
            hidden = _layer_norm_numpy(
                hidden + attention_output,
                self._weight(base + "attention.output.LayerNorm.weight"),
                self._weight(base + "attention.output.LayerNorm.bias"),
                epsilon,
            )
            intermediate = hidden @ self._weight(
                base + "intermediate.dense.weight"
            ).T + self._weight(base + "intermediate.dense.bias")
            activation = str(self.config.get("hidden_act") or "gelu")
            if activation not in {"gelu", "gelu_new"}:
                raise ValueError(f"unsupported BERT activation: {activation}")
            intermediate = _gelu_numpy(intermediate)
            output = intermediate @ self._weight(
                base + "output.dense.weight"
            ).T + self._weight(base + "output.dense.bias")
            hidden = _layer_norm_numpy(
                hidden + output,
                self._weight(base + "output.LayerNorm.weight"),
                self._weight(base + "output.LayerNorm.bias"),
                epsilon,
            )
        if self.pooling == "mean":
            mask = attention[..., None]
            pooled = (hidden * mask).sum(axis=1) / np.maximum(mask.sum(axis=1), 1.0)
        elif self.pooling == "cls":
            pooled = hidden[:, 0]
        elif self.pooling == "last-token":
            last = np.maximum(attention.sum(axis=1).astype(np.int64) - 1, 0)
            pooled = hidden[np.arange(len(hidden)), last]
        else:
            raise ValueError(f"unsupported pooling: {self.pooling}")
        if self.normalize:
            norms = np.linalg.norm(pooled, axis=1, keepdims=True)
            pooled = pooled / np.maximum(norms, np.finfo(np.float32).tiny)
        return pooled.astype(np.float32)

    def _forward_modernbert(
        self, input_ids: np.ndarray, attention: np.ndarray
    ) -> np.ndarray:
        batch, width = input_ids.shape
        head_size = self.hidden_size // self.heads
        if head_size % 2:
            raise ValueError("ModernBERT attention head size must be even")
        epsilon = float(self.config.get("norm_eps") or 1e-5)

        def layer_norm(values: np.ndarray, suffix: str) -> np.ndarray:
            bias = self._optional_weight(suffix + ".bias")
            if bias is None:
                bias = np.zeros(self.hidden_size, dtype=np.float32)
            return _layer_norm_numpy(
                values, self._weight(suffix + ".weight"), bias, epsilon
            )

        def linear(values: np.ndarray, suffix: str) -> np.ndarray:
            result = values @ self._weight(suffix + ".weight").T
            bias = self._optional_weight(suffix + ".bias")
            return result if bias is None else result + bias

        configured_types = self.config.get("layer_types")
        if configured_types is None:
            interval = int(self.config.get("global_attn_every_n_layers") or 3)
            layer_types = [
                "sliding_attention" if layer % interval else "full_attention"
                for layer in range(self.layers)
            ]
        else:
            layer_types = [str(value) for value in configured_types]
        if len(layer_types) != self.layers:
            raise ValueError("ModernBERT layer_types must match num_hidden_layers")

        rope_parameters = self.config.get("rope_parameters") or {}

        def rope_theta(layer_type: str) -> float:
            parameters = rope_parameters.get(layer_type) or {}
            if str(parameters.get("rope_type", "default")) != "default":
                raise ValueError("only default ModernBERT rotary embeddings are supported")
            if "rope_theta" in parameters:
                return float(parameters["rope_theta"])
            if layer_type == "full_attention":
                return float(self.config.get("global_rope_theta") or 160000.0)
            return float(self.config.get("local_rope_theta") or 10000.0)

        positions = np.broadcast_to(
            np.arange(width, dtype=np.float32), (batch, width)
        )
        hidden = self._weight("embeddings.tok_embeddings.weight")[input_ids]
        hidden = layer_norm(hidden, "embeddings.norm")
        key_mask = attention[:, None, None, :] > 0.0
        query_positions = np.arange(width)[:, None]
        key_positions = np.arange(width)[None, :]

        for layer, layer_type in enumerate(layer_types):
            base = f"layers.{layer}."
            attention_input = (
                hidden
                if layer == 0
                else layer_norm(hidden, base + "attn_norm")
            )
            qkv = linear(attention_input, base + "attn.Wqkv")
            qkv = qkv.reshape(batch, width, 3, self.heads, head_size)
            query, key, value = (
                qkv[:, :, index].transpose(0, 2, 1, 3) for index in range(3)
            )

            theta = rope_theta(layer_type)
            inv_frequency = 1.0 / (
                theta
                ** (
                    np.arange(0, head_size, 2, dtype=np.float32)
                    / float(head_size)
                )
            )
            frequencies = positions[..., None] * inv_frequency[None, None, :]
            rotary = np.concatenate((frequencies, frequencies), axis=-1)
            cosine = np.cos(rotary).astype(np.float32)[:, None, :, :]
            sine = np.sin(rotary).astype(np.float32)[:, None, :, :]

            def rotate_half(values: np.ndarray) -> np.ndarray:
                first, second = np.split(values, 2, axis=-1)
                return np.concatenate((-second, first), axis=-1)

            query = query * cosine + rotate_half(query) * sine
            key = key * cosine + rotate_half(key) * sine
            allowed = key_mask
            if layer_type == "sliding_attention":
                window = int(self.config.get("local_attention") or 128) // 2
                local = np.abs(query_positions - key_positions) <= window
                allowed = allowed & local[None, None, :, :]
            elif layer_type != "full_attention":
                raise ValueError(f"unsupported ModernBERT layer type: {layer_type}")
            attention_bias = np.where(allowed, 0.0, -1.0e9).astype(np.float32)
            scores = (query @ key.transpose(0, 1, 3, 2)) / math.sqrt(
                float(head_size)
            )
            probabilities = _softmax_numpy(scores + attention_bias)
            context = (
                (probabilities @ value).transpose(0, 2, 1, 3).reshape(hidden.shape)
            )
            hidden = hidden + linear(context, base + "attn.Wo")

            mlp_input = layer_norm(hidden, base + "mlp_norm")
            projected = linear(mlp_input, base + "mlp.Wi")
            inputs, gates = np.split(projected, 2, axis=-1)
            activation = str(self.config.get("hidden_activation") or "gelu")
            if activation == "gelu":
                activated = _gelu_numpy(inputs)
            elif activation in {"silu", "swish"}:
                activated = _silu_numpy(inputs)
            else:
                raise ValueError(
                    f"unsupported ModernBERT activation: {activation}"
                )
            hidden = hidden + linear(activated * gates, base + "mlp.Wo")

        hidden = layer_norm(hidden, "final_norm")
        if self.pooling == "mean":
            mask = attention[..., None]
            pooled = (hidden * mask).sum(axis=1) / np.maximum(mask.sum(axis=1), 1.0)
        elif self.pooling == "cls":
            pooled = hidden[:, 0]
        elif self.pooling == "last-token":
            last = np.maximum(attention.sum(axis=1).astype(np.int64) - 1, 0)
            pooled = hidden[np.arange(len(hidden)), last]
        else:
            raise ValueError(f"unsupported pooling: {self.pooling}")
        if self.normalize:
            norms = np.linalg.norm(pooled, axis=1, keepdims=True)
            pooled = pooled / np.maximum(norms, np.finfo(np.float32).tiny)
        return pooled.astype(np.float32)

    def encode_texts(
        self, texts: list[str], exact_length: int | None = None
    ) -> np.ndarray:
        return self.forward(*self.prepare_texts(texts, exact_length))

    def encode_documents(self, texts: list[str]) -> np.ndarray:
        rows = [self.document_token_ids(text) for text in texts]
        width = min(max(len(row) for row in rows), int(self.meta["max_tokens"]))
        ids = np.full((len(rows), width), self.pad_token_id, dtype=np.int64)
        mask = np.zeros((len(rows), width), dtype=np.float32)
        for index, row in enumerate(rows):
            retained = row[:width]
            ids[index, : len(retained)] = retained
            mask[index, : len(retained)] = 1.0
        return self.forward(ids, mask)


class MlxBertEncoder(NumpyBertEncoder):
    """MLX forward with the same converted weights and pooling contract."""

    def __init__(self, model_dir: Path):
        super().__init__(model_dir)
        try:
            import mlx.core as mx
        except ImportError as error:
            raise RuntimeError("MLX is required for the MLX encoder") from error
        self.mx = mx
        self.mx_weights = {
            name: mx.array(value) for name, value in self.weights.items()
        }

    def _mx_weight(self, suffix: str):
        return self.mx_weights[self.base + suffix]

    def forward_mx(self, input_ids, attention, *, weights=None, layer_indices=None):
        if self.architecture == "ModernBertModel":
            return self._forward_modernbert_mx(
                input_ids,
                attention,
                weights=weights,
                layer_indices=layer_indices,
            )
        mx = self.mx
        selected_weights = self.mx_weights if weights is None else weights

        def weight(suffix: str):
            return selected_weights[self.base + suffix]

        ids_numpy = np.asarray(input_ids, dtype=np.int64)
        mask_numpy = np.asarray(attention, dtype=np.float32)
        position_numpy = self._position_ids(ids_numpy, mask_numpy)
        ids = mx.array(ids_numpy)
        mask = mx.array(mask_numpy)
        positions = mx.array(position_numpy)
        token_types = mx.zeros_like(ids)
        hidden = mx.take(weight("embeddings.word_embeddings.weight"), ids, axis=0)
        hidden = hidden + mx.take(
            weight("embeddings.position_embeddings.weight"), positions, axis=0
        )
        hidden = hidden + mx.take(
            weight("embeddings.token_type_embeddings.weight"), token_types, axis=0
        )
        epsilon = float(self.config.get("layer_norm_eps") or 1e-12)

        def layer_norm(values, weight, bias):
            mean = mx.mean(values, axis=-1, keepdims=True)
            variance = mx.mean((values - mean) ** 2, axis=-1, keepdims=True)
            return ((values - mean) / mx.sqrt(variance + epsilon)) * weight + bias

        hidden = layer_norm(
            hidden,
            weight("embeddings.LayerNorm.weight"),
            weight("embeddings.LayerNorm.bias"),
        )
        batch, width = ids_numpy.shape
        head_size = self.hidden_size // self.heads
        attention_bias = (1.0 - mask[:, None, None, :]) * -1.0e9
        selected_layers = range(self.layers) if layer_indices is None else layer_indices
        for layer in selected_layers:
            base = f"encoder.layer.{layer}."
            projected = []
            for name in ("query", "key", "value"):
                projected.append(
                    hidden
                    @ mx.transpose(weight(base + f"attention.self.{name}.weight"))
                    + weight(base + f"attention.self.{name}.bias")
                )
            query, key, value = (
                mx.transpose(
                    mx.reshape(item, (batch, width, self.heads, head_size)),
                    (0, 2, 1, 3),
                )
                for item in projected
            )
            scores = (query @ mx.transpose(key, (0, 1, 3, 2))) / math.sqrt(
                float(head_size)
            )
            probabilities = mx.softmax(scores + attention_bias, axis=-1)
            context = mx.reshape(
                mx.transpose(probabilities @ value, (0, 2, 1, 3)), hidden.shape
            )
            attention_output = context @ mx.transpose(
                weight(base + "attention.output.dense.weight")
            ) + weight(base + "attention.output.dense.bias")
            hidden = layer_norm(
                hidden + attention_output,
                weight(base + "attention.output.LayerNorm.weight"),
                weight(base + "attention.output.LayerNorm.bias"),
            )
            intermediate = hidden @ mx.transpose(
                weight(base + "intermediate.dense.weight")
            ) + weight(base + "intermediate.dense.bias")
            intermediate = (
                0.5 * intermediate * (1.0 + mx.erf(intermediate / math.sqrt(2.0)))
            )
            output = intermediate @ mx.transpose(
                weight(base + "output.dense.weight")
            ) + weight(base + "output.dense.bias")
            hidden = layer_norm(
                hidden + output,
                weight(base + "output.LayerNorm.weight"),
                weight(base + "output.LayerNorm.bias"),
            )
        if self.pooling == "mean":
            expanded = mask[..., None]
            pooled = mx.sum(hidden * expanded, axis=1) / mx.maximum(
                mx.sum(expanded, axis=1), 1.0
            )
        elif self.pooling == "cls":
            pooled = hidden[:, 0]
        elif self.pooling == "last-token":
            last = np.maximum(mask_numpy.sum(axis=1).astype(np.int64) - 1, 0)
            pooled = mx.stack(
                [hidden[row, int(column)] for row, column in enumerate(last)]
            )
        else:
            raise ValueError(f"unsupported pooling: {self.pooling}")
        if self.normalize:
            pooled = pooled / mx.maximum(
                mx.sqrt(mx.sum(pooled * pooled, axis=1, keepdims=True)),
                np.finfo(np.float32).tiny,
            )
        return pooled

    def _forward_modernbert_mx(
        self, input_ids, attention, *, weights=None, layer_indices=None
    ):
        mx = self.mx
        selected_weights = self.mx_weights if weights is None else weights

        def weight(suffix: str):
            return selected_weights[self.base + suffix]

        def optional_weight(suffix: str):
            return selected_weights.get(self.base + suffix)

        ids_numpy = np.asarray(input_ids, dtype=np.int64)
        mask_numpy = np.asarray(attention, dtype=np.float32)
        ids = mx.array(ids_numpy)
        mask = mx.array(mask_numpy)
        batch, width = ids_numpy.shape
        head_size = self.hidden_size // self.heads
        if head_size % 2:
            raise ValueError("ModernBERT attention head size must be even")
        epsilon = float(self.config.get("norm_eps") or 1e-5)

        def layer_norm(values, suffix: str):
            mean = mx.mean(values, axis=-1, keepdims=True)
            variance = mx.mean((values - mean) ** 2, axis=-1, keepdims=True)
            result = ((values - mean) / mx.sqrt(variance + epsilon)) * weight(
                suffix + ".weight"
            )
            bias = optional_weight(suffix + ".bias")
            return result if bias is None else result + bias

        def linear(values, suffix: str):
            result = values @ mx.transpose(weight(suffix + ".weight"))
            bias = optional_weight(suffix + ".bias")
            return result if bias is None else result + bias

        configured_types = self.config.get("layer_types")
        if configured_types is None:
            interval = int(self.config.get("global_attn_every_n_layers") or 3)
            layer_types = [
                "sliding_attention" if layer % interval else "full_attention"
                for layer in range(self.layers)
            ]
        else:
            layer_types = [str(value) for value in configured_types]
        if len(layer_types) != self.layers:
            raise ValueError("ModernBERT layer_types must match num_hidden_layers")
        rope_parameters = self.config.get("rope_parameters") or {}

        def rope_theta(layer_type: str) -> float:
            parameters = rope_parameters.get(layer_type) or {}
            if str(parameters.get("rope_type", "default")) != "default":
                raise ValueError("only default ModernBERT rotary embeddings are supported")
            if "rope_theta" in parameters:
                return float(parameters["rope_theta"])
            if layer_type == "full_attention":
                return float(self.config.get("global_rope_theta") or 160000.0)
            return float(self.config.get("local_rope_theta") or 10000.0)

        hidden = mx.take(weight("embeddings.tok_embeddings.weight"), ids, axis=0)
        hidden = layer_norm(hidden, "embeddings.norm")
        positions = mx.arange(width, dtype=mx.float32)[None, :]
        key_mask = mask[:, None, None, :] > 0.0
        query_positions = mx.arange(width)[:, None]
        key_positions = mx.arange(width)[None, :]
        selected_layers = range(self.layers) if layer_indices is None else layer_indices

        for layer in selected_layers:
            layer_type = layer_types[layer]
            base = f"layers.{layer}."
            attention_input = (
                hidden
                if layer == 0
                else layer_norm(hidden, base + "attn_norm")
            )
            qkv = linear(attention_input, base + "attn.Wqkv")
            qkv = mx.reshape(qkv, (batch, width, 3, self.heads, head_size))
            query, key, value = (
                mx.transpose(qkv[:, :, index], (0, 2, 1, 3))
                for index in range(3)
            )

            theta = rope_theta(layer_type)
            exponent = mx.arange(0, head_size, 2, dtype=mx.float32) / float(
                head_size
            )
            inv_frequency = 1.0 / mx.power(mx.array(theta), exponent)
            frequencies = positions[..., None] * inv_frequency[None, None, :]
            rotary = mx.concatenate((frequencies, frequencies), axis=-1)
            cosine = mx.cos(rotary)[:, None, :, :]
            sine = mx.sin(rotary)[:, None, :, :]

            def rotate_half(values):
                first, second = mx.split(values, 2, axis=-1)
                return mx.concatenate((-second, first), axis=-1)

            query = query * cosine + rotate_half(query) * sine
            key = key * cosine + rotate_half(key) * sine
            allowed = key_mask
            if layer_type == "sliding_attention":
                window = int(self.config.get("local_attention") or 128) // 2
                local = mx.abs(query_positions - key_positions) <= window
                allowed = mx.logical_and(allowed, local[None, None, :, :])
            elif layer_type != "full_attention":
                raise ValueError(f"unsupported ModernBERT layer type: {layer_type}")
            attention_bias = mx.where(allowed, 0.0, -1.0e9)
            scores = (query @ mx.transpose(key, (0, 1, 3, 2))) / math.sqrt(
                float(head_size)
            )
            probabilities = mx.softmax(scores + attention_bias, axis=-1)
            context = mx.reshape(
                mx.transpose(probabilities @ value, (0, 2, 1, 3)), hidden.shape
            )
            hidden = hidden + linear(context, base + "attn.Wo")

            mlp_input = layer_norm(hidden, base + "mlp_norm")
            projected = linear(mlp_input, base + "mlp.Wi")
            inputs, gates = mx.split(projected, 2, axis=-1)
            activation = str(self.config.get("hidden_activation") or "gelu")
            if activation == "gelu":
                activated = 0.5 * inputs * (
                    1.0 + mx.erf(inputs / math.sqrt(2.0))
                )
            elif activation in {"silu", "swish"}:
                activated = inputs / (1.0 + mx.exp(-inputs))
            else:
                raise ValueError(
                    f"unsupported ModernBERT activation: {activation}"
                )
            hidden = hidden + linear(activated * gates, base + "mlp.Wo")

        hidden = layer_norm(hidden, "final_norm")
        if self.pooling == "mean":
            expanded = mask[..., None]
            pooled = mx.sum(hidden * expanded, axis=1) / mx.maximum(
                mx.sum(expanded, axis=1), 1.0
            )
        elif self.pooling == "cls":
            pooled = hidden[:, 0]
        elif self.pooling == "last-token":
            last = np.maximum(mask_numpy.sum(axis=1).astype(np.int64) - 1, 0)
            pooled = mx.stack(
                [hidden[row, int(column)] for row, column in enumerate(last)]
            )
        else:
            raise ValueError(f"unsupported pooling: {self.pooling}")
        if self.normalize:
            pooled = pooled / mx.maximum(
                mx.sqrt(mx.sum(pooled * pooled, axis=1, keepdims=True)),
                np.finfo(np.float32).tiny,
            )
        return pooled

    def forward(self, input_ids: np.ndarray, attention: np.ndarray) -> np.ndarray:
        output = self.forward_mx(input_ids, attention)
        self.mx.eval(output)
        return np.asarray(output, dtype=np.float32)


def cosine_similarity(left: np.ndarray, right: np.ndarray) -> float:
    left_flat = left.astype(np.float64, copy=False).ravel()
    right_flat = right.astype(np.float64, copy=False).ravel()
    denominator = np.linalg.norm(left_flat) * np.linalg.norm(right_flat)
    return float(np.dot(left_flat, right_flat) / denominator) if denominator else 0.0


def assert_embedding_match(
    reference: np.ndarray, candidate: np.ndarray, minimum_cosine: float = 0.999
) -> float:
    cosine = cosine_similarity(reference, candidate)
    if cosine < minimum_cosine:
        raise AssertionError(
            f"embedding cosine {cosine:.9f} is below required {minimum_cosine:.9f}"
        )
    return cosine


def _percentile(samples: list[float], fraction: float) -> float:
    if not samples:
        raise ValueError("latency sample is empty")
    ordered = sorted(samples)
    rank = round(fraction * (len(ordered) - 1))
    return float(ordered[rank])


def single_query_latency(
    encoder: NumpyBertEncoder,
    queries: list[str],
    token_length: int,
    *,
    warmups: int = 100,
    n: int = 10_000,
) -> dict[str, Any]:
    if not queries or n <= 0 or warmups < 0:
        raise ValueError("latency needs queries, n > 0, and warmups >= 0")
    started = time.perf_counter_ns()
    encoder.encode_texts([queries[0]], exact_length=token_length)
    cold_first_ms = (time.perf_counter_ns() - started) / 1_000_000.0
    for index in range(warmups):
        encoder.encode_texts([queries[index % len(queries)]], exact_length=token_length)
    samples = []
    for index in range(n):
        started = time.perf_counter_ns()
        encoder.encode_texts([queries[index % len(queries)]], exact_length=token_length)
        samples.append((time.perf_counter_ns() - started) / 1_000_000.0)
    original_lengths = [len(encoder.token_ids(query)) for query in queries]
    if all(length == token_length for length in original_lengths):
        adjustment = "none"
    elif all(length <= token_length for length in original_lengths):
        adjustment = "padded"
    elif all(length >= token_length for length in original_lengths):
        adjustment = "truncated"
    else:
        adjustment = "padded-or-truncated"
    return {
        "cold_first_ms": float(cold_first_ms),
        "p50_ms": _percentile(samples, 0.50),
        "p95_ms": _percentile(samples, 0.95),
        "p99_ms": _percentile(samples, 0.99),
        "warmups": float(warmups),
        "samples": float(n),
        "cycle_count": float(math.ceil(n / len(queries))),
        "token_length": float(token_length),
        "length_adjustment": adjustment,
    }


def calibrate_control(
    encoder: NumpyBertEncoder,
    queries: list[str],
    output: Path,
    *,
    token_length: int = 16,
    n: int = 1_000,
) -> dict[str, Any]:
    result = single_query_latency(encoder, queries, token_length, warmups=100, n=n)
    control = {
        "reference_p50_ms": result["p50_ms"],
        "tokens": float(token_length),
        "queries": float(len(queries)),
        "samples": float(n),
    }
    output.parent.mkdir(parents=True, exist_ok=True)
    output.write_text(
        json.dumps(control, indent=2, sort_keys=True) + "\n", encoding="utf-8"
    )
    return control


def compute_length_distribution(
    corpus_jsonl: list[Path], encoder: NumpyBertEncoder, output: Path
) -> dict[str, Any]:
    histogram: Counter[int] = Counter()
    sources = []
    for corpus in corpus_jsonl:
        if not corpus.is_file():
            raise FileNotFoundError(f"BEIR corpus is absent: {corpus}")
        documents = 0
        with corpus.open(encoding="utf-8") as source:
            for line in source:
                row = json.loads(line)
                text = " ".join(
                    part for part in (row.get("title", ""), row["text"]) if part
                )
                histogram[
                    min(
                        len(encoder.document_token_ids(text)),
                        int(encoder.meta["max_tokens"]),
                    )
                ] += 1
                documents += 1
        with corpus.open("rb") as source:
            digest = hashlib.file_digest(source, "sha256").hexdigest()
        sources.append(
            {"path": str(corpus), "sha256": digest, "documents": documents}
        )
    result = {
        "histogram": {
            str(length): count for length, count in sorted(histogram.items())
        },
        "documents": int(sum(histogram.values())),
        "sources": sources,
        "document_prefix": encoder.document_prefix,
    }
    output.parent.mkdir(parents=True, exist_ok=True)
    output.write_text(
        json.dumps(result, indent=2, sort_keys=True) + "\n", encoding="utf-8"
    )
    return result


def sample_lengths(path: Path, count: int, seed: int = 20260903) -> list[int]:
    data = json.loads(path.read_text(encoding="utf-8"))
    population = np.array([int(length) for length in data["histogram"]], dtype=np.int64)
    weights = np.array(list(data["histogram"].values()), dtype=np.float64)
    weights /= weights.sum()
    rng = np.random.default_rng(seed)
    return [int(value) for value in rng.choice(population, size=count, p=weights)]


def batched_throughput(
    encoder: NumpyBertEncoder,
    texts: list[str],
    lengths_path: Path,
    batch_sizes: tuple[int, ...] = (32, 128, 512),
    *,
    batches: int = 10,
    seed: int = 20260903,
) -> dict[str, dict[str, float]]:
    if not texts:
        raise ValueError("throughput needs at least one text")
    report = {}
    for batch_size in batch_sizes:
        lengths = sample_lengths(lengths_path, batch_size * batches, seed)
        real_tokens = 0
        padded_tokens = 0
        elapsed_ns = 0
        for batch_index in range(batches):
            selected = lengths[
                batch_index * batch_size : (batch_index + 1) * batch_size
            ]
            width = max(selected)
            batch_texts = [
                texts[(batch_index * batch_size + row) % len(texts)]
                for row in range(batch_size)
            ]
            ids = np.full((batch_size, width), encoder.pad_token_id, dtype=np.int64)
            mask = np.zeros((batch_size, width), dtype=np.float32)
            for row, (text, length) in enumerate(
                zip(batch_texts, selected, strict=True)
            ):
                source = encoder.document_token_ids(text)
                repeated = (source * math.ceil(length / len(source)))[:length]
                ids[row, :length] = repeated
                mask[row, :length] = 1.0
            started = time.perf_counter_ns()
            encoder.forward(ids, mask)
            elapsed_ns += time.perf_counter_ns() - started
            real_tokens += sum(selected)
            padded_tokens += batch_size * width
        elapsed = elapsed_ns / 1_000_000_000.0
        report[str(batch_size)] = {
            "real_tokens_s": float(real_tokens / elapsed),
            "padded_tokens_s": float(padded_tokens / elapsed),
            "real_tokens": float(real_tokens),
            "padded_tokens": float(padded_tokens),
            "seconds": float(elapsed),
        }
    return report


def _read_queries(path: Path) -> list[str]:
    with path.open(encoding="utf-8") as source:
        return [json.loads(line)["text"] for line in source if line.strip()]


def _encoder(model: Path, backend: str) -> NumpyBertEncoder:
    return MlxBertEncoder(model) if backend == "mlx" else NumpyBertEncoder(model)


def main() -> None:
    parser = argparse.ArgumentParser(description=__doc__)
    subparsers = parser.add_subparsers(dest="command", required=True)
    for name in ("latency", "calibrate-control", "throughput", "length-distribution"):
        command = subparsers.add_parser(name)
        command.add_argument("--model", required=True, type=Path)
        command.add_argument("--backend", choices=("mlx", "numpy"), default="mlx")
        command.add_argument("--out", required=True, type=Path)
    latency = subparsers.choices["latency"]
    latency.add_argument("--queries", required=True, type=Path)
    latency.add_argument("--tokens", required=True, type=int)
    latency.add_argument("--warmup", type=int, default=100)
    latency.add_argument("--n", type=int, default=10_000)
    latency.add_argument("--control", required=True, type=Path)
    latency.add_argument("--control-model", required=True, type=Path)
    latency.add_argument("--control-queries", required=True, type=Path)
    control = subparsers.choices["calibrate-control"]
    control.add_argument("--queries", required=True, type=Path)
    control.add_argument("--tokens", type=int, default=16)
    control.add_argument("--n", type=int, default=1_000)
    throughput = subparsers.choices["throughput"]
    throughput.add_argument("--queries", required=True, type=Path)
    throughput.add_argument("--lengths", required=True, type=Path)
    throughput.add_argument("--batch", default="32,128,512")
    throughput.add_argument("--batches", type=int, default=10)
    distribution = subparsers.choices["length-distribution"]
    distribution.add_argument("--corpus", required=True, type=Path, nargs="+")
    args = parser.parse_args()
    encoder = _encoder(args.model, args.backend)
    if args.command == "latency":
        control_data = json.loads(args.control.read_text(encoding="utf-8"))
        control_encoder = MlxBertEncoder(args.control_model)
        control_queries = _read_queries(args.control_queries)

        def measure_control() -> float:
            return single_query_latency(
                control_encoder,
                control_queries,
                int(control_data["tokens"]),
                warmups=100,
                n=int(control_data["samples"]),
            )["p50_ms"]

        bracketed = run_bracketed_cell(
            args.control,
            measure_control,
            lambda: single_query_latency(
                encoder,
                _read_queries(args.queries),
                args.tokens,
                warmups=args.warmup,
                n=args.n,
            ),
            measure_control,
        )
        result = bracketed.pop("cell_result")
        result["control"] = bracketed
        result["void"] = bracketed["void"]
        result["notes"] = bracketed["notes"]
        args.out.parent.mkdir(parents=True, exist_ok=True)
        args.out.write_text(
            json.dumps(result, indent=2, sort_keys=True) + "\n", encoding="utf-8"
        )
    elif args.command == "calibrate-control":
        calibrate_control(
            encoder,
            _read_queries(args.queries),
            args.out,
            token_length=args.tokens,
            n=args.n,
        )
    elif args.command == "throughput":
        result = batched_throughput(
            encoder,
            _read_queries(args.queries),
            args.lengths,
            tuple(int(item) for item in args.batch.split(",")),
            batches=args.batches,
        )
        args.out.write_text(
            json.dumps(result, indent=2, sort_keys=True) + "\n", encoding="utf-8"
        )
    else:
        compute_length_distribution(args.corpus, encoder, args.out)


if __name__ == "__main__":
    main()
