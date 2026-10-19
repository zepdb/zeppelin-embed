"""Release boundary checks: preserve the full text ABI only in arm64 full artifacts."""
import importlib.util
from pathlib import Path
import tempfile
import unittest

ROOT = Path(__file__).resolve().parents[2]


def load(name):
    spec = importlib.util.spec_from_file_location(name, Path(__file__).with_name(name + '.py'))
    module = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(module)
    return module


class TextPackaging(unittest.TestCase):
    def test_full_header_preserves_open_model_and_v2_abi(self):
        source = ROOT / 'crates/zeppelin-embed-ffi/include/zeppelin_embed.h'
        with tempfile.TemporaryDirectory() as tmp:
            destination = Path(tmp)
            load('core_header').write_artifact_headers(source, destination, 'graph-cypher')
            self.assertEqual((destination / 'zeppelin_embed.h').read_bytes(), source.read_bytes())
            for symbol in ('ze_text_open_model', 'ze_text_ingest_v2', 'ze_text_query_v2', 'ze_text_query_outcome_free'):
                self.assertIn(symbol, (destination / 'zeppelin_embed.h').read_text())

    def test_legacy_header_has_no_text_declarations(self):
        source = ROOT / 'crates/zeppelin-embed-ffi/include/zeppelin_embed.h'
        with tempfile.TemporaryDirectory() as tmp:
            destination = Path(tmp)
            load('core_header').write_artifact_headers(source, destination, 'legacy')
            contents = (destination / 'zeppelin_embed.h').read_text()
            self.assertNotIn('ZeText', contents)
            self.assertNotIn('ze_text_', contents)
            self.assertIn('ze_open(', contents)

    def test_full_exports_include_complete_text_abi(self):
        headers = load('core_header')
        checker = load('check-graph-artifact')
        self.assertTrue(headers.TEXT_FUNCTIONS <= checker.expected_symbols('graph-cypher'))
        self.assertFalse(any(s.startswith('ze_text_') for s in checker.expected_symbols('legacy')))


if __name__ == '__main__':
    unittest.main()
