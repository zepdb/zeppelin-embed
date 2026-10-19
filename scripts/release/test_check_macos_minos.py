"""Check deployment targets using real Mach-O archive members."""
from pathlib import Path
import subprocess
import sys
import tempfile
import unittest

CHECKER = Path(__file__).with_name('check-macos-minos.py')


@unittest.skipUnless(sys.platform == 'darwin', 'requires macOS Mach-O tools')
class CheckMacosMinos(unittest.TestCase):
    @classmethod
    def setUpClass(cls):
        cls.temporary = tempfile.TemporaryDirectory()
        cls.addClassCleanup(cls.temporary.cleanup)
        cls.root = Path(cls.temporary.name)
        source = cls.root / 'tiny.c'
        source.write_text('int tiny(void) { return 0; }\n')
        for target in ('13.0', '13.5'):
            subprocess.run([
                'xcrun', 'clang', '-c', f'-mmacosx-version-min={target}',
                str(source), '-o', str(cls.root / f'macos-{target}.o'),
            ], check=True)
        for name, members in (
            ('supported.a', ['macos-13.0.o']),
            ('mixed.a', ['macos-13.0.o', 'macos-13.5.o']),
            ('empty.a', ['macos-13.0.o']),
        ):
            subprocess.run(['ar', 'rcs', name, *members], cwd=cls.root, check=True)
        subprocess.run(['ar', '-d', 'empty.a', 'macos-13.0.o'], cwd=cls.root, check=True)

    def check_archive(self, name):
        return subprocess.run(
            [sys.executable, str(CHECKER), '13.0', str(self.root / name)],
            capture_output=True, text=True,
        )

    def test_archive_at_declared_target_passes(self):
        result = self.check_archive('supported.a')
        self.assertEqual(result.returncode, 0, result.stdout + result.stderr)

    def test_newer_archive_member_fails_and_names_member(self):
        result = self.check_archive('mixed.a')
        self.assertNotEqual(result.returncode, 0)
        self.assertIn('macos-13.5.o', result.stderr)
        self.assertIn('13.5', result.stderr)

    def test_input_without_macos_load_commands_fails(self):
        result = self.check_archive('empty.a')
        self.assertNotEqual(result.returncode, 0)
        self.assertIn('empty.a', result.stderr)
        self.assertIn('no macOS version load commands', result.stderr)


if __name__ == '__main__':
    unittest.main()
