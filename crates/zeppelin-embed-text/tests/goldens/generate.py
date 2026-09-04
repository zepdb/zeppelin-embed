#!/usr/bin/env python3
"""Regenerate the two checked-in fp32 architecture reference vectors."""

import argparse
import json
from pathlib import Path

import torch
from huggingface_hub import snapshot_download
from transformers import AutoConfig, AutoModel, AutoTokenizer


TEXT = "A bronze zeppelin crosses the winter sky."


def vector(model_id: str, prefix: str, trust_remote_code: bool) -> dict:
    root = snapshot_download(model_id)
    tokenizer = AutoTokenizer.from_pretrained(
        root, local_files_only=True, trust_remote_code=trust_remote_code
    )
    config = AutoConfig.from_pretrained(
        root, local_files_only=True, trust_remote_code=trust_remote_code
    )
    if trust_remote_code:
        config.unpad_inputs = False
        config.use_memory_efficient_attention = False
        config._attn_implementation = "eager"
    model = AutoModel.from_pretrained(
        root,
        config=config,
        local_files_only=True,
        trust_remote_code=trust_remote_code,
        dtype=torch.float32,
    ).cpu().eval()
    encoded = tokenizer(prefix + TEXT, return_tensors="pt")
    if trust_remote_code:
        encoded["position_ids"] = torch.arange(
            encoded["input_ids"].shape[1], dtype=torch.long
        ).unsqueeze(0)
    with torch.no_grad():
        states = model(**encoded).last_hidden_state
        if model_id == "MongoDB/mdbr-leaf-ir":
            mask = encoded["attention_mask"].unsqueeze(-1)
            pooled = (states * mask).sum(1) / mask.sum(1)
            dense_path = Path(root) / "2_Dense" / "model.safetensors"
            from safetensors.torch import load_file
            dense = load_file(dense_path)
            pooled = torch.nn.functional.linear(
                pooled, dense["linear.weight"], dense["linear.bias"]
            )
        else:
            pooled = states[:, 0]
        pooled = torch.nn.functional.normalize(pooled.float(), p=2, dim=1)
    return {
        "model": model_id,
        "text": TEXT,
        "prefix": prefix,
        "token_ids": encoded["input_ids"][0].tolist(),
        "attention_mask": encoded["attention_mask"][0].tolist(),
        "vector": pooled[0].tolist(),
    }


def main() -> None:
    output = Path(__file__).parent
    cases = {
        "mdbr_leaf_ir.json": vector(
            "MongoDB/mdbr-leaf-ir",
            "Represent this sentence for searching relevant passages: ",
            False,
        ),
        "arctic_m_v2.json": vector(
            "Snowflake/snowflake-arctic-embed-m-v2.0", "query: ", True
        ),
    }
    for name, values in cases.items():
        (output / name).write_text(
            json.dumps(values, indent=2) + "\n",
            encoding="utf-8",
        )


if __name__ == "__main__":
    main()
