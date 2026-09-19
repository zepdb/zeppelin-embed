#!/usr/bin/env python3
"""Directed manifest controls, using an isolated clone of a pinned checkout."""
import importlib.util
from pathlib import Path
import subprocess
import sys
import tempfile
import unittest

SOURCE = Path(sys.argv.pop(1))
SPEC = importlib.util.spec_from_file_location('binding_manifest', Path(__file__).parents[1] / 'cypher-binding-manifest.py')
MANIFEST = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(MANIFEST)

class ManifestControls(unittest.TestCase):
    @classmethod
    def setUpClass(cls):
        cls.scratch = tempfile.TemporaryDirectory(prefix='ze55-manifest-')
        cls.root = Path(cls.scratch.name)
        cls.source = cls.root / 'source'
        subprocess.run(['git', '-c', 'advice.detachedHead=false', 'clone', '--shared', '--quiet', str(SOURCE), str(cls.source)], check=True)
        subprocess.run(['git', '-C', str(cls.source), 'checkout', '--quiet', '--detach', MANIFEST.PIN], check=True)

    @classmethod
    def tearDownClass(cls):
        cls.scratch.cleanup()

    def test_changed_query_cannot_be_relabelled_as_original(self):
        original = MANIFEST.FIXTURE
        altered = self.root / 'altered.txt'
        text = original.read_text()
        altered.write_text(text.replace('RETURN', 'return', 1))
        MANIFEST.FIXTURE = altered
        try:
            with self.assertRaisesRegex(ValueError, 'original query'):
                MANIFEST.generate(self.source)
        finally:
            MANIFEST.FIXTURE = original

    def test_changed_source_refuses_even_with_unchanged_head(self):
        path = self.source / 'tck/features/clauses/match/Match1.feature'
        before = path.read_bytes()
        try:
            path.write_bytes(before + b'\n# modified\n')
            with self.assertRaisesRegex(ValueError, 'working source differs'):
                MANIFEST.generate(self.source)
        finally:
            path.write_bytes(before)

    def test_wrong_source_head_refuses_before_extraction(self):
        parent = subprocess.check_output(['git','-C',str(self.source),'-c','user.name=Fixture','-c','user.email=fixture@example.invalid','commit-tree',MANIFEST.PIN+'^{tree}','-m','Synthetic wrong source pin'],text=True).strip()
        try:
            subprocess.run(['git','-C',str(self.source),'update-ref','HEAD',parent],check=True)
            with self.assertRaisesRegex(ValueError, 'expected source'):
                MANIFEST.generate(self.source)
        finally:
            subprocess.run(['git','-C',str(self.source),'update-ref','HEAD',MANIFEST.PIN],check=True)

    def test_clean_inventory_keeps_execution_pending(self):
        result = MANIFEST.generate(self.source)
        self.assertEqual(len(result['scenarios']), 268)
        self.assertEqual(sum(row['support_disposition']=='supported' for row in result['scenarios']),99)
        self.assertEqual(sum(row['support_disposition']=='rejected_profile' for row in result['scenarios']),20)
        self.assertTrue(all(row['execution']['state']=='unexecuted' for row in result['scenarios']))
        self.assertEqual(result['binding_statements'], {'bind':152,'reject':3})

if __name__ == '__main__':
    unittest.main()
