#!/usr/bin/env python3
"""Pin model-only parity inputs and FP32 pack goldens, entirely offline.

Usage: <pack-venv>/bin/python generate_clean71.py --pack <extracted-pack>
Output: clean71.bin: for query then document, u32 row count, then per row
u32 original token count, u32 retained count, i32 token IDs, 768 f32 values.
All scalars are little endian. Vectors come from the pack, never MLX.
"""
import argparse
import gc
import json
import os
from pathlib import Path
import struct

os.environ["HF_HUB_OFFLINE"] = "1"
os.environ["TRANSFORMERS_OFFLINE"] = "1"

import numpy as np
import torch
from transformers import AutoConfig, AutoModel, AutoTokenizer


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--pack", type=Path, required=True)
    args = parser.parse_args()
    torch.set_num_threads(2)
    fixture = json.loads((args.pack / "fixtures/texts.json").read_text())
    output = bytearray()
    with np.load(args.pack / "fixtures/goldens.npz", allow_pickle=False) as goldens:
        # English, mixed Unicode, and the pack's explicitly over-limit query.
        for role, indices, prefix, limit in [("query", [0, 14, 15], "query: ", 96),
                                              ("document", [0, 14], "", 8192)]:
            root = args.pack / role
            tokenizer = AutoTokenizer.from_pretrained(root, local_files_only=True)
            config = AutoConfig.from_pretrained(root, trust_remote_code=True, local_files_only=True)
            if role == "document":
                config.use_memory_efficient_attention = False
                config.unpad_inputs = False
                config._attn_implementation = "eager"
            model = AutoModel.from_pretrained(root, config=config, trust_remote_code=True,
                                             local_files_only=True, torch_dtype=torch.float32).cpu().eval()
            texts = [(prefix + fixture[role][index]).strip() for index in indices]
            original = tokenizer(texts, truncation=False)["input_ids"]
            encoded = tokenizer(texts, padding=True, truncation=True, max_length=limit, return_tensors="pt")
            with torch.inference_mode():
                if role == "query":
                    vectors = model.encode_tokens(encoded["input_ids"], encoded["attention_mask"])
                else:
                    vectors = torch.nn.functional.normalize(model(**encoded).last_hidden_state[:, 0].float(), dim=-1)
            expected = goldens[role][indices]
            error = float(np.max(np.abs(vectors.numpy() - expected)))
            cosine = float(np.min(np.sum(vectors.numpy() * expected, axis=1)))
            assert error < 3e-5 and cosine > 0.99999, (role, error, cosine)
            output += struct.pack("<I", len(indices))
            for row, index in enumerate(indices):
                length = int(encoded["attention_mask"][row].sum())
                ids = encoded["input_ids"][row, :length].tolist()
                output += struct.pack("<II", len(original[row]), length)
                output += struct.pack("<" + "i" * length, *ids)
                output += expected[row].astype("<f4").tobytes()
            if role == "query":
                assert len(original[-1]) > 96 and int(encoded["attention_mask"][-1].sum()) == 96
            print(role, "pack indices", indices, "lengths", [len(ids) for ids in original],
                  "max_abs_error", error, "min_cosine", cosine)
            del model, vectors
            gc.collect()
    target = Path(__file__).with_name("clean71.bin")
    target.write_bytes(output)
    print(target.name, len(output), "bytes; torch", torch.__version__)


if __name__ == "__main__":
    main()
