#!/usr/bin/env python3
"""Exercise a real COFF archive with a short import object."""
import importlib.util
from pathlib import Path
import struct
import subprocess
import tempfile
import unittest

SCRIPT = Path(__file__).resolve().parents[1] / 'release/package-native.py'
spec = importlib.util.spec_from_file_location('pack', SCRIPT)
pack = importlib.util.module_from_spec(spec)
spec.loader.exec_module(pack)


class CoffMeasurement(unittest.TestCase):
    def test_measurement_preserves_imports_and_strips_debug(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            source, measured = root / 'fixture.lib', root / 'measured.lib'
            code, obj = root / 'engine.c', root / 'engine.obj'
            code.write_text('int ze_fixture(int x) { return x + 1; }\n')
            subprocess.run(['clang', '--target=x86_64-pc-windows-msvc', '-g',
                            '-c', str(code), '-o', str(obj)], check=True)
            payload = b'ExitProcess\0KERNEL32.dll\0'
            imported = root / 'kernel32.dll'
            imported.write_bytes(struct.pack('<HHHHIIHH', 0, 65535, 0, 0x8664,
                                              0, len(payload), 0, 4) + payload)
            subprocess.run([pack.llvm_tool('llvm-ar'), '--format=coff', 'qcsD',
                            str(source), str(obj), str(imported)], check=True)
            original = source.read_bytes()
            subprocess.run(['python3', str(SCRIPT), 'measure-coff', str(source),
                            str(measured)], check=True)
            self.assertEqual(source.read_bytes(), original)
            self.assertIn(imported.read_bytes(), measured.read_bytes())
            sections = subprocess.check_output([pack.llvm_tool('llvm-readobj'),
                                                '--sections', str(measured)], text=True)
            self.assertNotIn('Name: .debug', sections)
            symbols = subprocess.check_output([pack.llvm_tool('llvm-nm'),
                                               '--defined-only', str(measured)], text=True)
            self.assertIn('ze_fixture', symbols)
            self.assertIn('__imp_ExitProcess', symbols)
            sizes = subprocess.check_output([pack.llvm_tool('llvm-size'), '-A',
                                             str(measured)], text=True)
            self.assertIn('.text', sizes)


if __name__ == '__main__':
    unittest.main(verbosity=2)
