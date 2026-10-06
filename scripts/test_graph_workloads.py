"""Focused supervisor contracts; real public-worker proof is --smoke."""
import importlib.util
from pathlib import Path
import tempfile
import sys
import unittest
from unittest.mock import patch

spec = importlib.util.spec_from_file_location('workloads', Path(__file__).with_name('graph-workloads.py'))
w = importlib.util.module_from_spec(spec)
spec.loader.exec_module(w)


class Contracts(unittest.TestCase):
    def test_smoke_manifest_is_explicit_and_single_cell(self):
        m = w.smoke_manifest([])
        self.assertTrue(m['smoke'])
        self.assertEqual((m['warmups'], m['samples'], m['repetitions']), (5, 50, 1))
        self.assertEqual(m['cells'], ['baseline/A/rust/structured/project-evidence'])

    def test_missing_counter_fails_with_field_name(self):
        with self.assertRaisesRegex(ValueError, 'engine_peak_bytes'):
            w.require_counters({'counters': {}})

    def test_disk_refusal_precedes_copy(self):
        with tempfile.TemporaryDirectory() as d:
            src = Path(d) / 'source'; src.mkdir()
            (src / 'data').write_bytes(b'abc')
            with patch.object(w.shutil, 'disk_usage', return_value=type('Disk', (), {'free': 0})()):
                with self.assertRaisesRegex(ValueError, 'free disk'):
                    w.copy_store(src, Path(d) / 'copy')
            self.assertFalse((Path(d) / 'copy').exists())

    def test_real_blockers_do_not_name_landed_apis(self):
        m = w.accepted_manifest([])
        self.assertIn('ZE-313', m['blocked_inputs'])
        self.assertNotIn('ZE-278', m['blocked_inputs'])
        self.assertNotIn('ZE-76', m['blocked_inputs'])

    def test_abi_peak_is_not_mislabeled_as_pages(self):
        self.assertEqual(w.ABI_WORK_NAMES[22], 'query_reservation_peak_bytes')
        self.assertEqual(set(w.ABI_WORK_NAMES), set(range(23)))

    def test_public_workers_missing_exact_counters_are_named_blockers(self):
        row = {'counters': {key: 0 for key in w.RESOURCE_NAMES}}
        row['counters'].update({name: 0 for name in w.ABI_WORK_NAMES.values()})
        with self.assertRaisesRegex(ValueError, 'exact work counters for C and Swift workers require a public ledger decision'):
            w.require_counters(row, public_worker=True)
        self.assertEqual(w.accepted_manifest([])['blocked_inputs']['public-ledger-owner-decision'],
                         'exact work counters for C and Swift workers require a public ledger decision (ZE-76 chose internal)')

    def test_complete_payload_rejects_vector_and_text_corruption(self):
        expected = [{'id': '1267650600228229401496703205383', 'vector_bits': [1], 'text': None}]
        for changed in ({'id': '7', 'vector_bits': [1], 'text': None},
                        {'id': expected[0]['id'], 'vector_bits': [2], 'text': None},
                        {'id': expected[0]['id'], 'vector_bits': [1], 'text': ''}):
            with self.assertRaises(ValueError): w.oracle_check(expected, [changed], 9)

    def test_failed_process_is_recorded_never_a_pass(self):
        with tempfile.TemporaryDirectory() as d:
            output = Path(d) / 'failed'
            with self.assertRaisesRegex(ValueError, 'exit=3'):
                w.monitor_repetition([sys.executable, '-c', 'import sys; sys.exit(3)'], output,
                                     [sys.executable, '-c', 'import time; time.sleep(30)'], 5)
            import json
            failure = json.loads((output / 'repetition.json').read_text())
            self.assertFalse(failure['correct'])
            self.assertIn('exit 3', failure['error'])


if __name__ == '__main__':
    unittest.main()
