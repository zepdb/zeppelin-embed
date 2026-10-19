"""Offline exporter contract tests; no checkpoint or model execution required."""
import json
from pathlib import Path
import runpy
import struct
import tempfile
from types import SimpleNamespace
import unittest
from safetensors.torch import save_file

EXPORT = runpy.run_path(str(Path(__file__).with_name("ze-model")))


def source_tokenizer():
    special = ["[PAD]", "[UNK]", "[CLS]", "[SEP]", "[MASK]"]
    return {
        "normalizer": {"type": "BertNormalizer", "clean_text": True,
                       "handle_chinese_chars": True, "strip_accents": None, "lowercase": True},
        "pre_tokenizer": {"type": "BertPreTokenizer"},
        "model": {"type": "WordPiece", "vocab": {s: i for i, s in enumerate(special + ["cafe"])},
                  "unk_token": "[UNK]", "continuing_subword_prefix": "##", "max_input_chars_per_word": 100},
        "added_tokens": [{"id": i, "content": s, "single_word": False, "lstrip": False,
                          "rstrip": False, "normalized": False, "special": True}
                         for i, s in enumerate(special)],
        "post_processor": {"type": "TemplateProcessing", "single": [
            {"SpecialToken": {"id": "[CLS]", "type_id": 0}},
            {"Sequence": {"id": "A", "type_id": 0}},
            {"SpecialToken": {"id": "[SEP]", "type_id": 0}}],
            "special_tokens": {s: {"id": s, "ids": [special.index(s)], "tokens": [s]}
                               for s in ["[CLS]", "[SEP]"]}},
    }


class WordPieceExport(unittest.TestCase):
    def encode(self, tokenizer, config=None):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            (root / "tokenizer.json").write_text(json.dumps(tokenizer))
            if config is None:
                config = {"do_lower_case": tokenizer["normalizer"]["lowercase"]}
            (root / "tokenizer_config.json").write_text(json.dumps(config))
            return EXPORT["encode_tokenizer"](root, 1)

    def test_astra_08_export_records_effective_bert_normalization(self):
        self.assertEqual(self.encode(source_tokenizer())[:4], bytes([1, 3, 0, 0]))
        source = source_tokenizer()
        source["normalizer"].update(lowercase=False, strip_accents=True)
        self.assertEqual(self.encode(source)[:4], bytes([1, 2, 0, 0]))

    def test_astra_08_export_refuses_unrepresentable_tokenizers(self):
        for section, key, value in [
            ("normalizer", "clean_text", False),
            ("normalizer", "handle_chinese_chars", False),
            ("normalizer", "strip_accents", False),
            ("normalizer", "type", "NFKC"),
            ("pre_tokenizer", "type", "Whitespace"),
            ("model", "continuing_subword_prefix", "@@"),
            ("model", "max_input_chars_per_word", 200),
            ("model", "type", "BPE"),
        ]:
            with self.subTest(section=section, key=key):
                source = source_tokenizer()
                source[section][key] = value
                with self.assertRaises(ValueError):
                    self.encode(source)
        source = source_tokenizer()
        source["added_tokens"][0]["normalized"] = True
        with self.assertRaises(ValueError):
            self.encode(source)
        source = source_tokenizer()
        source["post_processor"]["single"].reverse()
        with self.assertRaises(ValueError):
            self.encode(source)

    def test_bake_writes_layout_two_with_one_tokenizer_per_tower(self):
        with tempfile.TemporaryDirectory() as directory:
            roots = [Path(directory) / name for name in ["document", "query"]]
            for index, root in enumerate(roots):
                root.mkdir()
                source = source_tokenizer()
                if index:
                    source["model"]["vocab"]["coffee"] = source["model"]["vocab"].pop("cafe")
                (root / "tokenizer.json").write_text(json.dumps(source))
                (root / "tokenizer_config.json").write_text("{}")
                (root / "config.json").write_text(json.dumps({
                    "model_type": "bert", "hidden_size": 2, "max_position_embeddings": 16,
                    "num_hidden_layers": 0, "num_attention_heads": 1,
                    "intermediate_size": 2, "layer_norm_eps": 1e-5,
                }))
                save_file({"embeddings.word_embeddings.weight": EXPORT["torch"].zeros(6, 2)},
                          root / "model.safetensors")
            args = SimpleNamespace(document=str(roots[0]), query=str(roots[1]),
                                   document_id="document", query_id="query", query_prefix="",
                                   alignment_digest="01", alpha=0.5, output=str(Path(directory) / "pair.zem"))
            EXPORT["bake"](args)
            body = Path(args.output).read_bytes()
            self.assertEqual(struct.unpack_from("<I", body, 8)[0], 2)
            self.assertEqual(body[16], 2)
            towers = [EXPORT["load_tower"](root, root.name, role, root.name, "")
                      for role, root in enumerate(roots)]
            start = 24 + sum(len(EXPORT["encode_tower"](tower)) for tower in towers)
            tokenizers = b"".join(EXPORT["encode_tokenizer"](root, 1) for root in roots)
            self.assertEqual(body[start:start + len(tokenizers)], tokenizers)
            self.assertEqual(struct.unpack_from("<d", body, start + len(tokenizers))[0], 0.5)
            self.assertEqual(EXPORT["xxhash"].xxh3_128_intdigest(body[:-16]).to_bytes(16, "little"), body[-16:])
            args.query = None
            args.output = str(Path(directory) / "symmetric.zem")
            EXPORT["bake"](args)
            body = Path(args.output).read_bytes()
            self.assertEqual(body[16], 1)
            start = 24 + len(EXPORT["encode_tower"](towers[0]))
            tokenizer = EXPORT["encode_tokenizer"](roots[0], 1)
            self.assertEqual(body[start:start + len(tokenizer)], tokenizer)
            self.assertEqual(struct.unpack_from("<d", body, start + len(tokenizer))[0], 0.5)

    def test_astra_08_export_refuses_conflicting_tokenizer_settings(self):
        for config in [{"do_lower_case": False}, {"strip_accents": False}, {"tokenize_chinese_chars": False},
                       {"padding_side": "left"}, {"truncation_side": "left"},
                       {"mask_token": "cafe"}, {"extra_special_tokens": {"extra": "cafe"}},
                       {"added_tokens_decoder": {"0": {"content": "cafe"}}}]:
            with self.subTest(config=config), self.assertRaises(ValueError):
                self.encode(source_tokenizer(), config)


if __name__ == "__main__":
    unittest.main()
