#!/usr/bin/env python3
"""Generate Step 08 FP32 source embeddings, including the pinned FiQA outlier."""
import argparse
import hashlib
import json
from pathlib import Path
import runpy
import subprocess

import sentence_transformers
from sentence_transformers import SentenceTransformer
import torch
import transformers

PINNED_WEIGHTS = {
    "query": "82691b5531ec8323546aa2246f8a5de073aff3bd0d7d98bec0619d2e51ee1297",
    "document": "463d27a3e88a748997316367c419bbdab4d809300a5f24b9b19ba9b2dc08ffdd",
}


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--query", type=Path, required=True)
    parser.add_argument("--document", type=Path, required=True)
    parser.add_argument("--fiqa-corpus", type=Path, required=True)
    parser.add_argument("--json-out", type=Path, required=True)
    parser.add_argument("--rust-out", type=Path, required=True)
    args = parser.parse_args()
    helper = runpy.run_path(str(Path(__file__).with_name("tokenizer-goldens.py")))
    outlier = next(json.loads(line) for line in args.fiqa_corpus.open()
                   if json.loads(line)["_id"] == "361279")
    outlier_text = outlier.get("title", "") + "\n" + outlier["text"]
    texts = ["Café déjà vu à la carte", outlier_text, "The bronze zeppelin."]
    torch.set_num_threads(4)
    report = {"torch": torch.__version__, "transformers": transformers.__version__,
              "sentence_transformers": sentence_transformers.__version__,
              "device": "cpu", "precision": "float32", "normalization": "L2", "roles": {}}
    lines = ["// Generated offline by tools/ze-model/embedding-goldens.py; source FP32, CLS, L2."]
    for role, root in [("query", args.query), ("document", args.document)]:
        helper["pinned_tokenizer"](root, role)
        with (root / "model.safetensors").open("rb") as source:
            weights_sha256 = hashlib.file_digest(source, "sha256").hexdigest()
        if weights_sha256 != PINNED_WEIGHTS[role]:
            raise ValueError(f"{role} weights do not match the pinned Step 08 source")
        model = SentenceTransformer(str(root), device="cpu", local_files_only=True)
        model.max_seq_length = 512
        prefix = helper["PREFIX"] if role == "query" else ""
        # Explicit prefix avoids dependence on SentenceTransformer prompt defaults.
        inputs = [prefix + text for text in texts]
        values = model.encode(inputs, prompt="", batch_size=3, normalize_embeddings=True,
                              show_progress_bar=False)
        tokens = model.tokenizer(inputs, padding=False, truncation=True, max_length=512)
        cases = [{"text": text, "ids": ids, "vector": vector.tolist()}
                 for text, ids, vector in zip(texts, tokens["input_ids"], values)]
        report["roles"][role] = {"source": str(root.resolve()), "prefix": prefix,
            "tokenizer_sha256": hashlib.sha256((root / "tokenizer.json").read_bytes()).hexdigest(),
            "weights_sha256": weights_sha256,
            "cases": cases}
        lines.append(f"pub const {role.upper()}_CASES: &[(&str, &[i32], &[f32])] = &[")
        for case in cases:
            lines.append(f"    ({helper['rust_string'](case['text'])}, &{case['ids']}, &{case['vector']}),")
        lines.append("];")
        del model
    args.json_out.write_text(json.dumps(report, ensure_ascii=False, indent=2) + "\n")
    args.rust_out.write_text("\n".join(lines) + "\n")
    subprocess.run(["rustfmt", "--edition", "2024", "--config", "skip_children=true", str(args.rust_out)], check=True)


if __name__ == "__main__":
    main()
