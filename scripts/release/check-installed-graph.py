#!/usr/bin/env python3
"""Qualify explicit distribution archives outside the checkout; never build Rust."""
import argparse
import hashlib
import importlib.util
import json
import os
import plistlib
from pathlib import Path
import re
import shutil
import subprocess
import tarfile
import tempfile
import unittest
import urllib.request
import zipfile

ROOT = Path(__file__).resolve().parents[2]


def pin(manifest, graph=False):
    marker = 'graph-xcframework-checksum' if graph else 'xcframework-checksum'
    matches = re.findall(r'"([0-9a-f]{64})"\s*// ze:' + marker + r'\b', manifest)
    if len(matches) != 1:
        raise ValueError(f'expected one readable {marker} pin')
    return matches[0]


def check_release_contract(root=ROOT, archive=None, enforce_checksum=False):
    manifest = (root / 'Package.swift').read_text()
    nested = (root / 'bindings/swift/graph/Package.swift').read_text()
    if '.library(name: "ZeppelinEmbedGraph", targets: ["ZeppelinEmbedGraph"])' not in manifest:
        raise ValueError('root_graph_product_is_installable: missing root graph product')
    checksum = pin(manifest, True)
    if checksum != pin(nested):
        raise ValueError('root and developer graph checksum pins differ')
    url = re.search(r'https://[^"\s]+/ZeppelinEmbedGraph.xcframework.zip', manifest)
    if not url or url.group() not in nested:
        raise ValueError('root and developer graph release URLs differ')
    if archive:
        actual = hashlib.sha256(archive.read_bytes()).hexdigest()
        if enforce_checksum and actual != checksum:
            raise ValueError(f'graph checksum mismatch: pinned={checksum} actual={actual}')
        return dict(pinned=checksum, actual=actual, matches=actual == checksum)
    return dict(pinned=checksum)


def run(command, **kwargs):
    print('+', ' '.join(map(str, command)), flush=True)
    return subprocess.run(list(map(str, command)), check=True, **kwargs)


def artifact_tools():
    spec = importlib.util.spec_from_file_location('artifact', Path(__file__).with_name('check-graph-artifact.py'))
    module = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(module)
    return module


def check_packaged_headers(headers, artifact, work):
    spec = importlib.util.spec_from_file_location('headers', Path(__file__).with_name('core_header.py'))
    module = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(module)
    expected = work / ('expected-' + artifact)
    module.write_artifact_headers(ROOT / 'crates/zeppelin-embed-ffi/include/zeppelin_embed.h', expected, artifact)
    for path in expected.iterdir():
        actual = headers / path.name
        if not actual.is_file() or actual.read_bytes() != path.read_bytes():
            raise ValueError(f'packaged header/module mismatch: {actual}')
    if artifact == 'legacy' and list(headers.glob('*graph*')):
        raise ValueError('legacy package advertises graph headers')


def check_xcframework(path, artifact, work, output):
    with (path / 'Info.plist').open('rb') as handle:
        libraries = plistlib.load(handle)['AvailableLibraries']
    arches = ['arm64'] if artifact == 'graph-cypher' else ['arm64', 'x86_64']
    if len(libraries) != 1 or sorted(libraries[0]['SupportedArchitectures']) != arches:
        raise ValueError(f'{artifact}: incorrect XCFramework architectures')
    entry = libraries[0]
    if entry.get('SupportedPlatform') != 'macos':
        raise ValueError('unexpected XCFramework platform')
    directory = path / entry['LibraryIdentifier']
    check_packaged_headers(directory / entry['HeadersPath'], artifact, work)
    library = directory / entry['LibraryPath']
    observed = subprocess.check_output(['lipo', '-archs', str(library)], text=True).split()
    if sorted(observed) != arches:
        raise ValueError('XCFramework library architectures disagree with plist')
    tools = artifact_tools()
    tools.check_exports(library, artifact)
    slices = {}
    for arch in arches:
        thin = work / (artifact + '-' + arch + '.a')
        if len(arches) == 1:
            shutil.copy(library, thin)
        else:
            run(['lipo', library, '-thin', arch, '-output', thin])
        tools.check_exports(thin, artifact)
        measured = tools.measure(thin, output)
        measured['budget_kib'] = 12288 if artifact == 'graph-cypher' else 5120
        measured['budget_pass'] = measured['section_kib'] <= measured['budget_kib']
        slices[arch] = measured
    return slices


def run_c_consumer(sdk, work, output):
    tools = artifact_tools()
    work.mkdir(parents=True, exist_ok=True)
    report = {}
    for kind in ('a', 'dylib'):
        library = sdk / 'lib' / ('libzeppelin_embed_graph_cypher_ffi.' + kind)
        binary = work / ('consumer-' + kind)
        for required in (sdk / 'include/zeppelin_embed.h', sdk / 'include/zeppelin_graph_contracts.h', library):
            if not required.is_file():
                raise FileNotFoundError(f'missing installed artifact input: {required}')
        tools.check_exports(library, 'graph-cypher')
        command = ['clang', '-std=c11', '-Wall', '-Wextra', '-Werror',
                   '-mmacosx-version-min=14.0', '-I', sdk / 'include',
                   work / 'consumer.c', library, '-framework', 'Security', '-liconv',
                   '-Wl,-rpath,' + str(sdk / 'lib'), '-o', binary]
        run(command, cwd=work)
        fixtures = work / ('fixtures-' + kind)
        fixtures.mkdir()
        env = os.environ.copy()
        env.pop('DYLD_LIBRARY_PATH', None)
        env.pop('DYLD_FALLBACK_LIBRARY_PATH', None)
        if kind == 'dylib':
            env['DYLD_PRINT_LIBRARIES'] = '1'
        result = run([binary, fixtures], cwd=work, env=env, capture_output=True, text=True)
        (output / ('c-' + kind + '.log')).write_text(result.stdout + result.stderr)
        if kind == 'dylib' and str(library) not in result.stderr:
            raise ValueError('installed dylib loaded path was not observed')
        report[kind] = dict(library=tools.measure(library, output), consumer=tools.measure(binary, output))
        if report[kind]['library']['section_kib'] > 12288:
            raise ValueError('graph FFI section budget exceeded')
    return report


def run_swift_consumer(xcframework, work, output, disable_swift_sandbox=False):
    package = work / 'package'
    package.mkdir()
    shutil.copytree(ROOT / 'bindings/swift/graph/Sources/ZeppelinEmbedGraph',
                    package / 'bindings/swift/graph/Sources/ZeppelinEmbedGraph')
    shutil.copytree(ROOT / 'bindings/swift/Sources/ZeppelinEmbed',
                    package / 'bindings/swift/Sources/ZeppelinEmbed')
    # Preserve the shipping product/target contract, changing only binary URLs
    # to explicitly extracted artifacts. No source headers or local-FFI mode.
    manifest = (ROOT / 'Package.swift').read_text()
    manifest = re.sub(r'url: "https://[^"\n]+/ZeppelinEmbedGraph.xcframework.zip",\s*checksum: graphBinaryChecksum',
                      'path: "Artifacts/ZeppelinEmbedGraph.xcframework"', manifest)
    legacy = work / 'ZeppelinEmbed.xcframework'
    # SwiftPM resolves both shipping binary targets; use the matching legacy zip.
    if not legacy.is_dir():
        raise ValueError('matching legacy XCFramework must be supplied')
    manifest = re.sub(r'url: "https://[^"\n]+/ZeppelinEmbed.xcframework.zip",\s*checksum: binaryChecksum',
                      'path: "Artifacts/ZeppelinEmbed.xcframework"', manifest)
    shutil.copytree(xcframework, package / 'Artifacts/ZeppelinEmbedGraph.xcframework')
    shutil.copytree(legacy, package / 'Artifacts/ZeppelinEmbed.xcframework')
    for path in ('bindings/swift/Tests/ZeppelinEmbedTests',):
        shutil.copytree(ROOT / path, package / path)
    (package / 'Package.swift').write_text(manifest)
    consumer = work / 'swift-consumer'
    (consumer / 'Sources/Consumer').mkdir(parents=True)
    shutil.copy(ROOT / 'bindings/swift/graph/Examples/InstalledConsumer/main.swift',
                consumer / 'Sources/Consumer/main.swift')
    (consumer / 'Package.swift').write_text('''// swift-tools-version: 5.10
import PackageDescription
let package = Package(name: "InstalledConsumer", platforms: [.macOS(.v14)],
 dependencies: [.package(path: "../package")], targets: [.executableTarget(name: "Consumer",
 dependencies: [.product(name: "ZeppelinEmbedGraph", package: "package")])])
''')
    env = {k: v for k, v in os.environ.items() if not k.startswith('ZE_')}
    env['CLANG_MODULE_CACHE_PATH'] = str(work / 'clang-cache')
    command = ['swift', 'build', '--package-path', consumer, '--scratch-path', work / 'swift-build',
               '--cache-path', work / 'swift-cache', '--jobs', '3']
    if disable_swift_sandbox:
        command.append('--disable-sandbox')
    manifest_path = package / 'Package.swift'
    manifest_path.write_text(manifest.replace(
        '.library(name: "ZeppelinEmbedGraph", targets: ["ZeppelinEmbedGraph"]),', ''))
    try:
        negative = subprocess.run(list(map(str, command)), env=env, cwd=work, capture_output=True, text=True)
        (output / 'root-product-red.log').write_text(negative.stdout + negative.stderr)
        if negative.returncode == 0 or "product 'ZeppelinEmbedGraph'" not in negative.stderr:
            raise ValueError('root product negative control did not reject the missing product')
    finally:
        manifest_path.write_text(manifest)
    run(command, env=env, cwd=work)
    binary = work / 'swift-build/debug/Consumer'
    result = run([binary, work / 'swift-fixture'], env=env, cwd=work, capture_output=True, text=True)
    (output / 'swift.log').write_text(result.stdout + result.stderr)
    return artifact_tools().measure(binary, output)


def run_legacy_consumers(args, output):
    report = {}
    if args.python_wheel:
        run(['python3', ROOT / 'bindings/python/tests/verify_wheel.py', args.python_wheel.resolve()])
        report['python'] = str(args.python_wheel.resolve())
    else:
        report['python'] = 'UNVERIFIED: no explicitly supplied wheel'
    if args.node_package:
        with tempfile.TemporaryDirectory(prefix='ze71-node-installed-') as directory:
            work = Path(directory).resolve()
            (work / 'package.json').write_text('{"private":true}')
            run(['npm', 'install', '--offline', '--ignore-scripts', '--no-audit', '--no-fund',
                 args.node_package.resolve()], cwd=work)
            shutil.copy(ROOT / 'bindings/node/test/installed-package-smoke.cjs', work / 'smoke.cjs')
            run(['node', 'smoke.cjs'], cwd=work)
        report['node'] = str(args.node_package.resolve())
    else:
        report['node'] = 'UNVERIFIED: no explicitly supplied Node package'
    return report


class ContractTests(unittest.TestCase):
    def test_root_graph_product_is_installable(self):
        check_release_contract()

    def test_graph_checksum_pin_is_readable(self):
        text = (ROOT / 'bindings/swift/graph/Package.swift').read_text()
        # This is the producer/release extraction, accepting marker whitespace.
        script = (ROOT / 'scripts/xcframework/build.sh').read_text()
        expression = re.search(r'grep -o "(.*?)" "\$MANIFEST"', script)
        self.assertIsNotNone(expression)
        pattern = expression.group(1).replace('\\"', '"').replace('$PIN_MARKER', 'xcframework-checksum')
        result = subprocess.run(['grep', '-o', pattern], input=text, text=True, capture_output=True)
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertEqual(len(pin(text)), 64)

    def test_xcframework_failure_stops_packaging(self):
        script = (ROOT / 'scripts/xcframework/build.sh').read_text()
        statement = re.search(r'^xcodebuild -create-xcframework.*?(?=\n\n)', script, re.S | re.M).group()
        result = subprocess.run(['bash', '-c',
            'xcodebuild() { return 42; }; macos_archive=x; HEADERS_DIR=x; ARTIFACT=x;\n'
            + statement + '\nexit 0'], capture_output=True, text=True)
        self.assertNotEqual(result.returncode, 0, 'packaging continued after xcodebuild failed')

    def test_corrupt_checksum_is_rejected(self):
        with tempfile.TemporaryDirectory() as directory:
            archive = Path(directory) / 'corrupt.zip'
            archive.write_bytes(b'corrupt')
            with self.assertRaisesRegex(ValueError, 'checksum mismatch'):
                check_release_contract(archive=archive, enforce_checksum=True)


def qualify(args):
    output = args.output.resolve()
    output.mkdir(parents=True, exist_ok=True)
    archive = args.artifact_root / 'xcframework-graph-cypher/ZeppelinEmbedGraph.xcframework.zip'
    sdk_archive = args.artifact_root / 'macos-sdk-graph-cypher/zeppelin-embed-graph-cypher-macos-arm64.tar.gz'
    for packaged in (archive, sdk_archive):
        if not packaged.is_file():
            raise FileNotFoundError('ZE-71 missing matching installed graph artifact: ' + str(packaged))
    report = dict(contract=check_release_contract(archive=archive, enforce_checksum=args.enforce_checksum),
                  os=subprocess.check_output(['sw_vers'], text=True),
                  hardware=subprocess.check_output(['uname', '-a'], text=True),
                  structured_query='UNQUALIFIED: ZE-278 Swift structured query/search/options/get wrappers missing; C structured exports are present')
    with tempfile.TemporaryDirectory(prefix='ze71-installed-') as directory:
        work = Path(directory).resolve()
        with tarfile.open(sdk_archive) as tar:
            tar.extractall(work, filter='data')
        with zipfile.ZipFile(args.legacy_xcframework or args.artifact_root / 'xcframework/ZeppelinEmbed.xcframework.zip') as legacy_zip:
            legacy_zip.extractall(work)
        with zipfile.ZipFile(archive) as zip_file:
            zip_file.extractall(work)
        sdk = work / 'zeppelin-embed-graph-cypher-macos-arm64'
        shutil.copy(ROOT / 'crates/zeppelin-embed-ffi/tests/c/graph_artifact_consumer.c', work / 'consumer.c')
        check_packaged_headers(sdk / 'include', 'graph-cypher', work)
        report['xcframeworks'] = {identity: check_xcframework(work / name, identity, work, output)
            for identity, name in [('legacy', 'ZeppelinEmbed.xcframework'), ('graph-cypher', 'ZeppelinEmbedGraph.xcframework')]}
        dylib = sdk / 'lib/libzeppelin_embed_graph_cypher_ffi.dylib'
        if subprocess.check_output(['lipo', '-archs', str(dylib)], text=True).split() != ['arm64']:
            raise ValueError('graph SDK dylib must be arm64 only')
        deployment = subprocess.check_output(['vtool', '-show-build', str(dylib)], text=True)
        if not re.search(r'\bminos 14\.0\b', deployment):
            raise ValueError('graph SDK deployment target must remain macOS 14.0')
        report['deployment'] = deployment
        report['c'] = run_c_consumer(sdk, work, output)
        report['swift'] = run_swift_consumer(work / 'ZeppelinEmbedGraph.xcframework', work, output, args.disable_swift_sandbox)
        # Substituting a valid legacy archive must fail exact graph exports.
        legacy_library = next((work / 'ZeppelinEmbed.xcframework').rglob('*.a'))
        try:
            artifact_tools().check_exports(legacy_library, 'graph-cypher')
        except RuntimeError as error:
            if 'missing=' not in str(error):
                raise
            report['legacy_substitution'] = str(error)
        else:
            raise ValueError('legacy substitution was accepted as graph')
        # Missing packaged inputs must reject even with source headers present.
        for missing in ('include/zeppelin_embed.h', 'lib/libzeppelin_embed_graph_cypher_ffi.a'):
            path = sdk / missing
            saved = path.read_bytes()
            path.unlink()
            try:
                try:
                    negative = work / 'negative'
                    negative.mkdir(exist_ok=True)
                    shutil.copy(work / 'consumer.c', negative / 'consumer.c')
                    run_c_consumer(sdk, negative, output)
                except FileNotFoundError as error:
                    if 'missing installed artifact input' not in str(error):
                        raise
                else:
                    raise ValueError('missing installed input accepted: ' + missing)
            finally:
                path.write_bytes(saved)
    report['legacy_languages'] = run_legacy_consumers(args, output)
    (output / 'report.json').write_text(json.dumps(report, indent=2) + '\n')
    if any(not measured['budget_pass'] for slices in report['xcframeworks'].values() for measured in slices.values()):
        raise ValueError('installed execution completed; section budget failed (see report.json)')


def remote_qualify(args):
    """Read-only post-publication proof; never tag, upload, or change pins."""
    if not re.fullmatch(r'v\d+\.\d+\.\d+', args.remote_tag):
        raise ValueError('remote tag must be an exact vMAJOR.MINOR.PATCH release')
    output = args.output.resolve()
    output.mkdir(parents=True, exist_ok=True)
    base = 'https://github.com/zepdb/zeppelin-embed/releases/download/' + args.remote_tag + '/'
    with tempfile.TemporaryDirectory(prefix='ze71-remote-') as directory:
        work = Path(directory).resolve()
        def download(url, destination):
            destination.parent.mkdir(parents=True, exist_ok=True)
            with urllib.request.urlopen(url) as response, destination.open('wb') as handle:
                shutil.copyfileobj(response, handle)
        raw = 'https://raw.githubusercontent.com/zepdb/zeppelin-embed/' + args.remote_tag + '/'
        for manifest in ('Package.swift', 'bindings/swift/graph/Package.swift'):
            download(raw + manifest, work / manifest)
        archive = work / 'ZeppelinEmbedGraph.xcframework.zip'
        download(base + archive.name, archive)
        contract = check_release_contract(work, archive, True)
        sdk_archive = work / 'zeppelin-embed-graph-cypher-macos-arm64.tar.gz'
        download(base + sdk_archive.name, sdk_archive)
        with tarfile.open(sdk_archive) as tar:
            tar.extractall(work, filter='data')
        shutil.copy(ROOT / 'crates/zeppelin-embed-ffi/tests/c/graph_artifact_consumer.c', work / 'consumer.c')
        report = dict(contract=contract, c=run_c_consumer(
            work / 'zeppelin-embed-graph-cypher-macos-arm64', work, output))
        consumer = work / 'consumer'
        (consumer / 'Sources/Consumer').mkdir(parents=True)
        shutil.copy(ROOT / 'bindings/swift/graph/Examples/InstalledConsumer/main.swift',
                    consumer / 'Sources/Consumer/main.swift')
        (consumer / 'Package.swift').write_text(
            '// swift-tools-version: 5.10\nimport PackageDescription\n'
            'let package = Package(name: "RemoteConsumer", platforms: [.macOS(.v14)],'
            'dependencies: [.package(url: "https://github.com/zepdb/zeppelin-embed.git", exact: "'
            + args.remote_tag[1:] + '")], targets: [.executableTarget(name: "Consumer",'
            'dependencies: [.product(name: "ZeppelinEmbedGraph", package: "zeppelin-embed")])])\n')
        env = {k: v for k, v in os.environ.items() if not k.startswith('ZE_')}
        env['CLANG_MODULE_CACHE_PATH'] = str(work / 'clang-cache')
        run(['swift', 'build', '--package-path', consumer, '--scratch-path', work / 'build',
             '--cache-path', work / 'cache', '--jobs', '3'], env=env, cwd=work)
        run([work / 'build/debug/Consumer', work / 'fixture'], env=env, cwd=work)
        report['swift'] = artifact_tools().measure(work / 'build/debug/Consumer', output)
        report['scope'] = 'remote batch/Cypher only; structured query requires ZE-241/ZE-278'
        (output / 'remote-report.json').write_text(json.dumps(report, indent=2) + '\n')


if __name__ == '__main__':
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--remote-tag', help='read-only post-publication proof against actual remote assets')
    parser.add_argument('--self-test', action='store_true')
    parser.add_argument('--artifact-root', type=Path, default=ROOT / 'target')
    parser.add_argument('--output', type=Path, default=ROOT / 'tasks/evidence/ze-71-installed')
    parser.add_argument('--disable-swift-sandbox', action='store_true', help='explicit local workaround when nested sandbox-exec is unavailable')
    parser.add_argument('--legacy-xcframework', type=Path, help='explicit legacy zip for local qualification, still size-gated')
    parser.add_argument('--python-wheel', type=Path)
    parser.add_argument('--node-package', type=Path)
    parser.add_argument('--enforce-checksum', action='store_true', help='CI release bytes must match both manifest pins')
    args = parser.parse_args()
    if args.self_test:
        unittest.main(argv=[__file__])
    elif args.remote_tag:
        remote_qualify(args)
    else:
        qualify(args)
