"""Release boundary checks: preserve the full text ABI only in arm64 full artifacts."""
import importlib.util
from pathlib import Path
import re
import tempfile
import unittest
from unittest.mock import Mock, patch

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
            for symbol in ('ze_text_default_model', 'ZeTextModelRef', 'ze_text_open_model', 'ze_text_ingest_v2', 'ze_text_query_v2', 'ze_text_query_outcome_free'):
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
        checker = load('check-graph-artifact')
        source = ROOT / 'crates/zeppelin-embed-ffi/include/zeppelin_embed.h'
        declared = set(re.findall(r'\b(ze_text_\w+)\s*\(', source.read_text()))
        expected = {s for s in checker.expected_symbols('graph-cypher') if s.startswith('ze_text_')}
        self.assertEqual(declared, expected)
        self.assertFalse(any(s.startswith('ze_text_') for s in checker.expected_symbols('legacy')))

    def test_graph_export_profiles_require_exact_symbols(self):
        checker = load('check-graph-artifact')
        graph = {s for s in (ROOT / 'crates/zeppelin-embed-ffi/symbols.graph.allowlist').read_text().splitlines()
                 if s.startswith('ze_') and not s.startswith('ze_text_')}
        for text in (False, True):
            for suffix in ('.a', '.dylib'):
                with self.subTest(text=text, suffix=suffix):
                    expected = graph.copy()
                    if text:
                        expected |= checker.TEXT_FUNCTIONS
                        expected |= checker.TEXT_STATIC_INTERNALS if suffix == '.a' else checker.MLX_DYLIB_GLOBALS
                    invalid = [expected - {'ze_open'}, expected | {'ze_unexpected'},
                               expected - {'ze_text_open'} if text else expected | {'ze_text_open'}]
                    with patch.object(checker, 'load_packager', return_value=Mock()), \
                         patch.object(checker.subprocess, 'run') as run:
                        run.return_value.stdout = '\n'.join(f'00000000 T _{s}' for s in expected)
                        checker.check_exports(Path('graph' + suffix), 'graph-cypher', text=text)
                        for symbols in invalid:
                            run.return_value.stdout = '\n'.join(f'00000000 T _{s}' for s in symbols)
                            with self.assertRaisesRegex(RuntimeError, 'exports:'):
                                checker.check_exports(Path('graph' + suffix), 'graph-cypher', text=text)


if __name__ == '__main__':
    unittest.main()
