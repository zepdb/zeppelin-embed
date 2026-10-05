#!/usr/bin/env python3
"""ZE-59 negative controls for public receipts and verifier integrity."""
import importlib.util
from pathlib import Path
import subprocess
import sys
import tempfile
import unittest

ROOT = Path(__file__).resolve().parents[2]

class ConformanceControls(unittest.TestCase):
    @classmethod
    def setUpClass(cls):
        spec = importlib.util.spec_from_file_location('profile', ROOT / 'scripts/cypher-profile-conformance.py')
        cls.profile = importlib.util.module_from_spec(spec)
        spec.loader.exec_module(cls.profile)

    def receipts(self):
        lines = []
        for row in self.profile.corpus():
            text = (self.profile.FIXTURES / row['fixture']).read_text()
            body = next(block.split('\n', 1)[1] for block in text.split('\n=== ')[1:]
                        if block.split('\n', 1)[0] == row['coordinate'])
            lines.append(f'ZE59\t{row["coordinate"]}\tGREEN\t{body.encode().hex()}\tcontrol')
        return lines

    def test_missing_receipt_is_rejected(self):
        with self.assertRaisesRegex(ValueError, 'incomplete receipts'):
            self.profile.merge_receipts(self.profile.corpus(), '\n'.join(self.receipts()[:-1]), 'control', 'control')

    def test_duplicate_receipt_is_rejected(self):
        lines = self.receipts()
        with self.assertRaisesRegex(ValueError, 'duplicate'):
            self.profile.merge_receipts(self.profile.corpus(), '\n'.join(lines + lines[:1]), 'control', 'control')

    def test_changed_expectation_is_rejected(self):
        lines = self.receipts()
        fields = lines[0].split('\t')
        fields[3] = (bytes.fromhex(fields[3]) + b'\n  | null |').hex()
        lines[0] = '\t'.join(fields)
        with self.assertRaisesRegex(ValueError, 'stale receipt'):
            self.profile.merge_receipts(self.profile.corpus(), '\n'.join(lines), 'control', 'control')

    def test_failed_and_interrupted_inventory_is_rejected(self):
        with self.assertRaises(ValueError):
            self.profile.check_inventory('test result: FAILED')
        with self.assertRaisesRegex(ValueError, 'unreached'):
            self.profile.check_inventory('test result: ok. 0 passed;')

    def test_frozen_profile_rejects_temporary_fixture_edit(self):
        # Temporary disk edit, restored even if the control itself fails.
        path = self.profile.FIXTURES / 'read-tck-execution.txt'
        original = path.read_bytes()
        frozen = self.profile.corpus()
        try:
            path.write_bytes(original.replace(b'  MATCH (n)', b'  MATCH (changed)', 1))
            self.assertNotEqual(self.profile.corpus()[0]['hashes']['query'], frozen[0]['hashes']['query'])
            with self.assertRaisesRegex(ValueError, 'stale receipt'):
                self.profile.merge_receipts(self.profile.corpus(), '\n'.join(self.receipts_for_frozen(original)), 'control', 'control')
        finally:
            path.write_bytes(original)

    def receipts_for_frozen(self, original):
        lines = self.receipts()
        block = original.decode().split('\n=== ')[1].split('\n', 1)[1]
        fields = lines[0].split('\t')
        fields[3] = block.encode().hex()
        lines[0] = '\t'.join(fields)
        return lines

    def test_source_expectation_control_catches_changed_result(self):
        # A synthetic parser control; never counted as original TCK evidence.
        with tempfile.TemporaryDirectory(prefix='ze59-source-control-') as directory:
            root = Path(directory)
            (root / 'read-tck-execution.txt').write_text('\n=== local/control.feature [1]\nquery\n  RETURN $x AS v\nparameter x = 1\nexpect bag\n  | v |\n  | 1 |\nside-effects none\n')
            (root / 'write-tck-execution.txt').write_text('')
            body = ('  Scenario: [1] control\n    And parameters are:\n      | x | 1 |\n'
                    '    When executing query:\n      """\n      RETURN $x AS v\n      """\n'
                    '    Then the result should be, in any order:\n      | v |\n      | 1 |\n    And no side effects\n')
            saved = self.profile.FIXTURES
            self.profile.FIXTURES = root
            try:
                self.profile.verify_expectations({'local/control.feature [1]': {'original_body': body}})
                with self.assertRaisesRegex(ValueError, 'altered original results'):
                    self.profile.verify_expectations({'local/control.feature [1]': {'original_body': body.replace('| 1 |\n    And', '| 2 |\n    And')}})
            finally:
                self.profile.FIXTURES = saved

    def test_pinned_unordered_list_spelling_preserves_rows(self):
        with tempfile.TemporaryDirectory(prefix='ze59-list-control-') as directory:
            root = Path(directory)
            (root / 'read-tck-execution.txt').write_text(
                '\n=== local/control.feature [1]\nquery\n  RETURN [1, 2] AS v\n'
                'expect bag-lists-unordered\n  | v |\n  | [1, 2] |\nside-effects none\n')
            (root / 'write-tck-execution.txt').write_text('')
            body = ('  Scenario: [1] control\n    When executing query:\n'
                    '      """\n      RETURN [1, 2] AS v\n      """\n'
                    '    Then the result should be (ignoring element order for lists):\n'
                    '      | v |\n      | [1, 2] |\n    And no side effects\n')
            saved = self.profile.FIXTURES
            self.profile.FIXTURES = root
            try:
                self.profile.verify_expectations({'local/control.feature [1]': {'original_body': body}})
                with self.assertRaisesRegex(ValueError, 'altered original results'):
                    self.profile.verify_expectations({'local/control.feature [1]': {
                        'original_body': body.replace('| [1, 2] |', '| [1, 3] |')}})
            finally:
                self.profile.FIXTURES = saved

    def test_public_execution_requires_receipts(self):
        command = ['cargo', 'test', '-p', 'zeppelin-embed-cypher', '--test',
                   'read_tck_execution', '--test', 'write_tck_execution',
                   'original_', '--', '--nocapture', '--test-threads=1']
        result = subprocess.run(command, cwd=ROOT, capture_output=True, text=True)
        log = result.stdout + result.stderr
        (ROOT / 'tasks/evidence/ze-59/receipt-control.log').write_text(log)
        self.assertEqual(result.returncode, 0, log[-5000:])
        receipts = [line for line in log.splitlines() if line.startswith('ZE59\t')]
        self.assertEqual(len(receipts), 99, 'missing per-scenario public execution receipts')

if __name__ == '__main__':
    if len(sys.argv) > 1 and not sys.argv[1].startswith('ConformanceControls'):
        sys.argv.pop(1)  # source checkout is used by verifier controls below
    unittest.main()
