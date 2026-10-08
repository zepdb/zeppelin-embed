#!/usr/bin/env python3
"""ZE-259: inspect real shipping artifacts, not build-script source text."""
import argparse
from collections import Counter
from pathlib import Path
import re
import subprocess
import sys
import unittest

parser = argparse.ArgumentParser()
parser.add_argument('--archive', action='append', nargs=2,
                    metavar=('CARGO_ARCHIVE', 'DISTRIBUTION_ARCHIVE'), default=[])
parser.add_argument('--addon', action='append', default=[])
args = parser.parse_args()
if not args.archive or not args.addon:
    parser.error('provide at least one --archive and --addon')
libdir = Path(subprocess.check_output(['rustc', '--print', 'target-libdir'], text=True).strip())
suffix = '.exe' if sys.platform == 'win32' else ''

def llvm(name, *arguments):
    return subprocess.check_output(
        [str(libdir.parent / 'bin' / (name + suffix)), *map(str, arguments)], text=True
    )

class NativePackaging(unittest.TestCase):
    def test_static_archives_have_no_embedded_bitcode(self):
        for original, archive in args.archive:
            with self.subTest(archive=archive):
                sections = llvm('llvm-readobj', '--sections', archive)
                self.assertNotRegex(sections, r'Name: (?:__bitcode|__cmdline|\.llvmbc|\.llvmcmd)(?: |\n)')
                symbols = llvm('llvm-nm', '--quiet', '--extern-only', '--defined-only', archive)
                self.assertRegex(symbols, r'\b_?ze_store_graph_apply\b')
                def defined(path):
                    output = llvm('llvm-nm', '--quiet', '--extern-only', '--defined-only',
                                  '--no-llvm-bc', '--format=just-symbols', path)
                    return Counter(line for line in output.splitlines() if line and not line.endswith(':'))
                self.assertEqual(defined(original), defined(archive),
                                 'archive packaging changed native definitions/import symbols')

    def test_addons_export_only_napi_registration(self):
        for addon in args.addon:
            with self.subTest(addon=addon):
                if Path(addon).read_bytes()[:2] == b'MZ':
                    exports = llvm('llvm-readobj', '--coff-exports', addon)
                    names = set(re.findall(r'^  Name: (.+)$', exports, re.M))
                else:
                    names = set(llvm('llvm-nm', '--extern-only', '--defined-only',
                                     '--format=just-symbols', addon).splitlines())
                    names = {name.removeprefix('_') for name in names}
                self.assertEqual(names, {'napi_register_module_v1', 'node_api_module_get_api_version_v1'})

    def test_addons_are_stripped(self):
        for addon in args.addon:
            with self.subTest(addon=addon):
                if Path(addon).read_bytes()[:2] == b'MZ':
                    headers = llvm('llvm-readobj', '--file-headers', addon)
                    self.assertRegex(headers, r'SymbolCount: 0\b')
                    # MSVC keeps small POGO/feature records; forbid only a PDB link.
                    debug = llvm('llvm-readobj', '--coff-debug-directory', addon)
                    self.assertNotIn('IMAGE_DEBUG_TYPE_CODEVIEW', debug)
                else:
                    symbols = llvm('llvm-readobj', '--symbols', addon)
                    for symbol in symbols.split('  Symbol {')[1:]:
                        # Apple's strip retains this linker marker, even with -S -x.
                        self.assertTrue('Extern\n' in symbol or 'Name: radr://5614542 ' in symbol,
                                        symbol[:200])

unittest.main(argv=[sys.argv[0]], verbosity=2)
