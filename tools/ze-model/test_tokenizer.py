"""Offline exporter contract tests; no checkpoint or model execution required."""
import json
from pathlib import Path
import runpy
import tempfile
from types import SimpleNamespace
import unittest
from unittest.mock import patch

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

    def test_astra_08_bundle_refuses_distinct_tokenizer_contracts(self):
        with tempfile.TemporaryDirectory() as directory:
            roots = [Path(directory) / name for name in ["document", "query"]]
            for index, root in enumerate(roots):
                root.mkdir()
                source = source_tokenizer()
                if index:
                    source["model"]["vocab"]["coffee"] = source["model"]["vocab"].pop("cafe")
                (root / "tokenizer.json").write_text(json.dumps(source))
                (root / "tokenizer_config.json").write_text("{}")
            args = SimpleNamespace(document=str(roots[0]), query=str(roots[1]),
                                   document_id="document", query_id="query", query_prefix="",
                                   alignment_digest="01", alpha=0.5)
            def load_tower(root, *unused):
                return SimpleNamespace(root=root, architecture=1, tensors=())
            def serialize_tower(*unused):
                raise AssertionError("distinct tokenizers reached bundle serialization")
            with patch.dict(EXPORT["bake"].__globals__, load_tower=load_tower, encode_tower=serialize_tower):
                with self.assertRaises(ValueError):
                    EXPORT["bake"](args)

    def test_astra_08_export_refuses_conflicting_tokenizer_settings(self):
        for config in [{"do_lower_case": False}, {"strip_accents": False}, {"tokenize_chinese_chars": False},
                       {"padding_side": "left"}, {"truncation_side": "left"},
                       {"mask_token": "cafe"}, {"extra_special_tokens": {"extra": "cafe"}},
                       {"added_tokens_decoder": {"0": {"content": "cafe"}}}]:
            with self.subTest(config=config), self.assertRaises(ValueError):
                self.encode(source_tokenizer(), config)


if __name__ == "__main__":
    unittest.main()
