"""Offline ArcticQuery export contract; no trained weights required."""
import json
from pathlib import Path
import runpy
import struct
import tempfile
import unittest

from safetensors.torch import save_file
import torch

EXPORT = runpy.run_path(str(Path(__file__).with_name("ze-model")))


class ArcticQueryExport(unittest.TestCase):
    def test_arctic_query_exports_rank_projection_and_query_limit(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            (root / "config.json").write_text(json.dumps({
                "model_type": "arctic_query", "hidden_size": 4, "embedding_rank": 2,
                "output_dimension": 6, "max_position_embeddings": 256,
                "num_hidden_layers": 1, "num_attention_heads": 1,
                "intermediate_size": 6, "layer_norm_eps": 1e-12, "rope_theta": 160000,
            }))
            (root / "config_sentence_transformers.json").write_text(
                json.dumps({"prompts": {"query": "query: "}}))
            (root / "sentence_bert_config.json").write_text('{"max_seq_length": 96}')
            save_file({"embeddings.word_embeddings.weight": torch.zeros(7, 2),
                       "embeddings.embedding_projection.weight": torch.zeros(4, 2),
                       "output_projection.weight": torch.zeros(6, 4)}, root / "model.safetensors")
            tower = EXPORT["load_tower"](root, "tiny-arctic", 1, "query", "query: ")
            encoded = EXPORT["encode_tower"](tower)
            self.assertEqual(tower.architecture, 3)
            # Skip the variable-length identity fields to the embedding dimensions.
            offset = 1
            for _ in range(3):
                size = struct.unpack_from("<I", encoded, offset)[0]
                offset += 4 + size
            self.assertEqual(struct.unpack_from("<I", encoded, offset)[0], 6)
            offset += 4 + 2 + 4 + len("query: ")
            self.assertEqual(struct.unpack_from("<I", encoded, offset)[0], 96)
            self.assertEqual(struct.unpack_from("<II", encoded, len(encoded) - 8), (2, 6))
            self.assertEqual(tower.pooling, 2)
            self.assertEqual(struct.unpack_from("<I", encoded, len(encoded) - 28)[0], 1)


if __name__ == "__main__":
    unittest.main()
