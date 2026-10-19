"""Offline release assembly: real bake, compiler boundary replaced by fixed bytes."""
import hashlib
import json
from pathlib import Path
import runpy
import struct
import tempfile
import unittest
from unittest.mock import patch

import torch
from safetensors.torch import save_file
from test_tokenizer import source_tokenizer

EXPORT = runpy.run_path(str(Path(__file__).with_name("ze-model")))


class Release(unittest.TestCase):
    def test_fp16_tensor_bytes_and_descriptor(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            save_file({"weight": torch.tensor([1.25, -0.5])}, root / "model.safetensors")
            tensor, = EXPORT["load_tensors"](root, "document", 0, "fp16")
            self.assertEqual(tensor.data, struct.pack("<ee", 1.25, -0.5))
            descriptor = EXPORT["tensor_descriptor"](tensor, 4096)
            self.assertEqual(descriptor[4 + len(tensor.name)], 1)

    def test_release_query_mlx_defaults_to_document_precision(self):
        self.check_releases((("fp16", None, "fp16"), ("fp32", None, "fp16")))

    def test_release_query_mlx_explicit_override_wins(self):
        self.check_releases((("fp16", "fp32", "int8"), ("fp32", "fp16", "fp16")))

    def check_releases(self, cases):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            pack = root / "pack"
            pack.mkdir()
            (pack / "PAIR.json").write_text(json.dumps({
                "document": {"name": "arctic-m-v2", "path": "document"},
                "query": {"name": "clean71-v1", "path": "query", "prefix": "query: "}}))
            for role in ("document", "query"):
                tower = pack / role
                tower.mkdir()
                config = {"model_type": "bert" if role == "document" else "arctic_query",
                          "hidden_size": 4, "max_position_embeddings": 8192,
                          "num_hidden_layers": 0, "num_attention_heads": 1,
                          "intermediate_size": 8, "layer_norm_eps": 1e-5,
                          "embedding_rank": 2, "output_dimension": 4}
                (tower / "config.json").write_text(json.dumps(config))
                tokenizer = source_tokenizer()
                if role == "query":
                    tokenizer.update(normalizer=None, added_tokens=[])
                    tokenizer["model"] = {"type": "Unigram", "vocab": [
                        [token, -1.] for token in ["<pad>", "<unk>", "<s>", "</s>", "a", "b"]]}
                (tower / "tokenizer.json").write_text(json.dumps(tokenizer))
                (tower / "tokenizer_config.json").write_text("{}")
                (tower / "config_sentence_transformers.json").write_text(
                    json.dumps({"prompts": {"query": "query: "}}))
                save_file({"embeddings.word_embeddings.weight": torch.ones(6, 4)},
                          tower / "model.safetensors")

            def compile_query(args):
                self.assertEqual(args.tokens, 96)
                compiled_precisions.append(getattr(args, "precision", None))
                self.assertEqual(Path(args.query), (pack / "query").resolve())
                compiled = Path(args.output).with_suffix(".mlmodelc")
                (compiled / "weights").mkdir(parents=True)
                (compiled / "weights/weight.bin").write_bytes(b"compiled query")

            for precision, override, coreml_precision in cases:
                query_precision = override or precision
                snapshots = []
                for run in range(2):
                    out = root / f"{precision}-{override}-{run}"
                    args = EXPORT["parser"]().parse_args([
                        "release", "--pack", str(pack), "--out", str(out),
                        "--name", "tiny", "--document-precision", precision,
                        "--query-coreml-precision", coreml_precision,
                        *(["--query-mlx-precision", override] if override else [])])
                    compiled_precisions = []
                    with patch.dict(EXPORT["release"].__globals__, coreml_export=compile_query):
                        EXPORT["release"](args)
                    bundle = (out / "tiny.zem").read_bytes()
                    name = b"query/embeddings.word_embeddings.weight"
                    descriptor = bundle.index(struct.pack("<I", len(name)) + name)
                    self.assertEqual(bundle[descriptor + 4 + len(name)],
                                     {"fp32": 0, "fp16": 1}[query_precision])
                    self.assertEqual(compiled_precisions, [coreml_precision])
                    readme = (out / "README.md").read_text()
                    self.assertIn(f"{query_precision.upper()} MLX query weights", readme)
                    self.assertIn(f"CoreML query: {coreml_precision}", readme)
                    manifest = json.loads((out / "zeppelin-model.json").read_text())
                    self.assertEqual(manifest["query_coreml"], "tiny.mlmodelc")
                    self.assertEqual([f["path"] for f in manifest["files"]],
                                     ["tiny.mlmodelc/weights/weight.bin", "tiny.zem"])
                    for entry in manifest["files"]:
                        data = (out / entry["path"]).read_bytes()
                        self.assertEqual(entry["size"], len(data))
                        self.assertEqual(entry["sha256"], hashlib.sha256(data).hexdigest())
                    for line in (out / "SHA256SUMS").read_text().splitlines():
                        digest, path = line.split("  ")
                        self.assertEqual(digest, hashlib.sha256((out / path).read_bytes()).hexdigest())
                    snapshots.append({p.relative_to(out).as_posix(): p.read_bytes()
                                      for p in out.rglob("*") if p.is_file()})
                    with self.assertRaises(FileExistsError):
                        EXPORT["release"](args)
                self.assertEqual(snapshots[0], snapshots[1])


if __name__ == "__main__":
    unittest.main()
