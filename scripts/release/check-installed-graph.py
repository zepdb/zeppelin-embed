#!/usr/bin/env python3
"""Qualify explicit distribution archives outside the checkout; never rebuild the engine."""
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
import sys
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
    if 'ZeppelinEmbedGraph' in manifest:
        raise ValueError('legacy root package must not resolve graph bytes')
    if '.library(name: "ZeppelinEmbed", targets: ["ZeppelinEmbed"])' not in manifest:
        raise ValueError('missing legacy root product')
    pin(manifest)  # Preserve the independently pinned legacy binary.
    if '.library(name: "ZeppelinEmbedGraph", targets: ["ZeppelinEmbedGraph"])' not in nested:
        raise ValueError('missing separate graph product')
    checksum = pin(nested)
    if not re.search(r'https://[^"\s]+/ZeppelinEmbedGraph.xcframework.zip', nested):
        raise ValueError('missing separate graph release URL')
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
        # Owner decisions: graph 2026-09-27; legacy 2026-10-05, all slices.
        measured['budget_kib'] = 12288 if artifact == 'graph-cypher' else 5632
        measured['budget_pass'] = measured['section_kib'] <= measured['budget_kib']
        slices[arch] = measured
    return slices


def consumer_receipt(stdout):
    """ZE-71 producer handoff: a receipt from this installed execution's log.

    The producer, not this checker, owns structured-query/get implementation.
    Existing batch/Cypher consumers cannot synthesize structured reachability.
    """
    prefix = 'ZE_GRAPH_INSTALLED_RECEIPT\t'
    receipts = [json.loads(line[len(prefix):]) for line in stdout.splitlines() if line.startswith(prefix)]
    if len(receipts) != 1:
        return dict(state='blocked', missing_input='ZE-71/ZE-278 installed execution receipt')
    return receipts[0]


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
        report[kind] = dict(library=tools.measure(library, output), consumer=tools.measure(binary, output),
                            reachability=consumer_receipt(result.stdout))
        if report[kind]['library']['section_kib'] > 12288:
            raise ValueError('graph FFI section budget exceeded')
    return report


def run_rust_consumer(sdk, work, output):
    library = sdk / 'lib/libzeppelin_embed_graph_cypher_ffi.a'
    for required in (sdk / 'include/zeppelin_embed.h',
                     sdk / 'include/zeppelin_graph_contracts.h', library):
        if not required.is_file():
            raise FileNotFoundError(f'missing installed artifact input: {required}')
    source = work / 'installed.rs'
    shutil.copy(ROOT / 'scripts/release/consumers/installed.rs', source)
    obj = work / 'rust-fixture.o'
    run(['clang', '-std=c11', '-Wall', '-Wextra', '-Werror', '-mmacosx-version-min=14.0',
         '-Dmain=ze_installed_fixture', '-I', sdk / 'include', '-c', work / 'consumer.c', '-o', obj])
    binary = work / 'rust-consumer'
    env = dict(os.environ, MACOSX_DEPLOYMENT_TARGET='14.0')
    run(['rustc', '--edition=2024', source, '-C', 'link-arg=' + str(obj),
         '-C', 'link-arg=' + str(library), '-l', 'framework=Security', '-l', 'iconv', '-o', binary], env=env)
    fixture = work / 'rust-fixture'
    fixture.mkdir()
    result = run([binary, fixture], cwd=work, capture_output=True, text=True)
    (output / 'rust.log').write_text(result.stdout + result.stderr)
    return dict(artifact_tools().measure(binary, output), reachability=consumer_receipt(result.stdout))


def run_swift_consumer(xcframework, work, output, disable_swift_sandbox=False):
    package = work / 'package'
    package.mkdir()
    shutil.copytree(ROOT / 'bindings/swift/graph/Sources', package / 'Sources')
    shutil.copytree(ROOT / 'bindings/swift/graph/Examples', package / 'Examples')
    shutil.copytree(ROOT / 'bindings/swift/graph/Tests', package / 'Tests')
    # Exercise the separate shipping manifest with only its binary URL replaced.
    manifest = (ROOT / 'bindings/swift/graph/Package.swift').read_text()
    manifest, replaced = re.subn(
        r'url:\s*"https://[^"\n]+/ZeppelinEmbedGraph.xcframework.zip",\s*checksum: binaryChecksum',
        'path: "Artifacts/ZeppelinEmbedGraph.xcframework"', manifest)
    if replaced != 1:
        raise ValueError('expected one separate graph binary target')
    shutil.copytree(xcframework, package / 'Artifacts/ZeppelinEmbedGraph.xcframework')
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
        (output / 'graph-product-red.log').write_text(negative.stdout + negative.stderr)
        if negative.returncode == 0 or "product 'ZeppelinEmbedGraph'" not in negative.stderr:
            raise ValueError('graph package product negative control did not reject the missing product')
    finally:
        manifest_path.write_text(manifest)
    run(command, env=env, cwd=work)
    binary = work / 'swift-build/debug/Consumer'
    result = run([binary, work / 'swift-fixture'], env=env, cwd=work, capture_output=True, text=True)
    (output / 'swift.log').write_text(result.stdout + result.stderr)
    return dict(artifact_tools().measure(binary, output), reachability=consumer_receipt(result.stdout))


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
    def test_installed_receipts_require_rust_and_resources(self):
        receipt = dict(executed=['batch', 'structured', 'get', 'cypher'],
                       artifact_kind='graph-cypher', exit_status=0, resources=True)
        report = {'reachability': {k: receipt for k in ('c-static', 'c-dylib', 'swift')}}
        with self.assertRaisesRegex(ValueError, 'rust'):
            require_structured_execution(report)
        report['reachability']['rust'] = dict(receipt, resources=False)
        with self.assertRaisesRegex(ValueError, 'rust'):
            require_structured_execution(report)
        report['reachability']['rust'] = receipt
        require_structured_execution(report)

    def test_graph_package_is_the_only_graph_product(self):
        check_release_contract()
        self.assertNotIn('ZeppelinEmbedGraph', (ROOT / 'Package.swift').read_text())
        self.assertEqual(len(pin((ROOT / 'Package.swift').read_text())), 64)

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

    def test_profile_import_from_another_cwd(self):
        script = ROOT / 'scripts/graph-profile-parity.py'
        code = (
            'import importlib.util, sys; '
            'spec = importlib.util.spec_from_file_location("graph_profile", sys.argv[1]); '
            'profile = importlib.util.module_from_spec(spec); '
            'spec.loader.exec_module(profile); '
            'assert callable(profile.validate_swift_tests)'
        )
        with tempfile.TemporaryDirectory() as directory:
            subprocess.run([sys.executable, '-I', '-c', code, str(script)],
                           cwd=directory, check=True)

    def test_corrupt_checksum_is_rejected(self):
        with tempfile.TemporaryDirectory() as directory:
            archive = Path(directory) / 'corrupt.zip'
            archive.write_bytes(b'corrupt')
            with self.assertRaisesRegex(ValueError, 'checksum mismatch'):
                check_release_contract(archive=archive, enforce_checksum=True)


def run_c_profile_consumer(sdk, work, output):
    import importlib.util
    script = ROOT / 'scripts/graph-profile-parity.py'
    spec = importlib.util.spec_from_file_location('graph_profile', script)
    profile = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(profile)
    manifest = profile.build_manifest()
    runner = ROOT / 'target/debug/graph-profile'
    if not runner.is_file():
        raise ValueError('ZE-74 profile runner missing; build graph-profile before installed qualification')
    cases = [c for c in json.loads(subprocess.check_output([runner, 'describe'], text=True))['cases'] if c['id'].startswith('local/')]
    # Retain the independently authored column mapping after primitive expansion.
    authored = {c['id']: c for c in manifest['local']}
    for case in cases:
        case['column_mapping'] = authored[case['id']].get('column_mapping', {})
    (work / 'graph_profile_cases.h').write_text(profile.generate_c(manifest))
    report = []
    for flavor, suffix in [('static', 'a'), ('dylib', 'dylib')]:
        library = sdk / ('lib/libzeppelin_embed_graph_cypher_ffi.' + suffix)
        if not library.is_file():
            raise ValueError('BLOCKED ZE-71: installed graph ' + flavor + ' input missing')
        consumer = work / ('profile-' + flavor)
        run(['clang', '-std=c11', '-Wall', '-Wextra', '-Werror', '-arch', 'arm64', '-mmacosx-version-min=14.0',
             '-I', sdk / 'include', '-I', work, ROOT / 'crates/zeppelin-embed-ffi/tests/c/graph_profile_parity.c',
             library, '-framework', 'Security', '-liconv', '-Wl,-rpath,' + str(sdk / 'lib'), '-o', consumer])
        fixture = work / ('profile-fixture-' + flavor)
        fixture.mkdir()
        result = run([consumer, fixture], capture_output=True, text=True)
        (output / ('profile-' + flavor + '.jsonl')).write_text(result.stdout)
        observations = [json.loads(line) for line in result.stdout.splitlines()]
        profile.compare_c_consumer(cases, observations)
        report.append(dict(consumer='installed-c-' + flavor, state='focused GREEN',
                           manifest_sha256=manifest['manifest_sha256'], artifact_sha256=profile.digest(library),
                           observations=observations))
    return report


def profile_receipt_state(path):
    header = (ROOT / 'crates/zeppelin-embed-ffi/include/zeppelin_graph_contracts.h').read_text()
    if 'ze_graph_query(' not in header:
        raise ValueError('required public C structured query export is absent')
    if path is None:
        return 'BLOCKED: C structured source present; ZE-71 installed profile receipts and ZE-278 Swift structured receipts missing'
    receipts = json.loads(path.read_text())
    if not receipts:
        raise ValueError('ZE-74 installed profile receipts are empty')
    required = {'installed-c-static', 'installed-c-dylib', 'installed-swift'}
    seen = {r.get('consumer') for r in receipts}
    if not required.issubset(seen):
        raise ValueError('ZE-71 installed profile consumers missing: ' + str(required - seen))
    for r in receipts:
        if r.get('state') != 'focused GREEN' or not r.get('artifact_sha256') or not r.get('manifest_sha256'):
            raise ValueError('stale or incomplete installed profile receipt')
    return {'state': 'focused GREEN', 'receipts': str(path), 'release': 'BLOCKED: ZE-72 closure and macOS 14 runtime receipt required'}


def require_structured_execution(report):
    for kind in ('c-static', 'c-dylib', 'swift', 'rust'):
        receipt = report.get('reachability', {}).get(kind, {})
        if (receipt.get('executed') != ['batch', 'structured', 'get', 'cypher']
                or receipt.get('artifact_kind') != 'graph-cypher' or receipt.get('exit_status') != 0
                or receipt.get('resources') is not True):
            raise ValueError('missing ZE-71/ZE-278 installed structured query/get receipt for '
                             + kind + '; ze_graph_query is present; see report.json')


def qualify(args):
    output = args.output.resolve()
    output.mkdir(parents=True, exist_ok=True)
    archive = args.artifact_root / 'xcframework-graph-cypher/ZeppelinEmbedGraph.xcframework.zip'
    sdk_archive = args.artifact_root / 'macos-sdk-graph-cypher/zeppelin-embed-graph-cypher-macos-arm64.tar.gz'
    if not archive.is_file() or not sdk_archive.is_file():
        missing = [str(p) for p in (archive, sdk_archive) if not p.is_file()]
        (output / 'report.json').write_text(json.dumps(dict(state='blocked', missing_input='ZE-71', missing_artifacts=missing), indent=2)+'\n')
        raise ValueError('BLOCKED ZE-71: rebuilt installed graph SDK/XCFramework inputs missing')
    report = dict(contract=check_release_contract(archive=archive, enforce_checksum=args.enforce_checksum),
                  os=subprocess.check_output(['sw_vers'], text=True),
                  hardware=subprocess.check_output(['uname', '-a'], text=True),
                  structured_query=profile_receipt_state(args.profile_receipts))
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
        report['rust'] = run_rust_consumer(sdk, work, output)
        report['c_profile'] = run_c_profile_consumer(sdk, work, output)
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
    report['reachability'] = {**{'c-' + kind: report['c'][key]['reachability']
                              for kind, key in [('static', 'a'), ('dylib', 'dylib')]},
                              'swift': report['swift']['reachability'], 'rust': report['rust']['reachability']}
    report['structured_query'] = report['reachability']
    report['legacy_languages'] = run_legacy_consumers(args, output)
    (output / 'report.json').write_text(json.dumps(report, indent=2) + '\n')
    require_structured_execution(report)
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
        checkout = work / 'release'
        run(['git', 'clone', '--depth', '1', '--branch', args.remote_tag,
             'https://github.com/zepdb/zeppelin-embed.git', checkout], cwd=work)
        archive = work / 'ZeppelinEmbedGraph.xcframework.zip'
        download(base + archive.name, archive)
        contract = check_release_contract(checkout, archive, True)
        sdk_archive = work / 'zeppelin-embed-graph-cypher-macos-arm64.tar.gz'
        download(base + sdk_archive.name, sdk_archive)
        with tarfile.open(sdk_archive) as tar:
            tar.extractall(work, filter='data')
        shutil.copy(ROOT / 'crates/zeppelin-embed-ffi/tests/c/graph_artifact_consumer.c', work / 'consumer.c')
        sdk = work / 'zeppelin-embed-graph-cypher-macos-arm64'
        report = dict(contract=contract, c=run_c_consumer(sdk, work, output),
                      rust=run_rust_consumer(sdk, work, output))
        consumer = work / 'consumer'
        (consumer / 'Sources/Consumer').mkdir(parents=True)
        shutil.copy(ROOT / 'bindings/swift/graph/Examples/InstalledConsumer/main.swift',
                    consumer / 'Sources/Consumer/main.swift')
        (consumer / 'Package.swift').write_text(
            '// swift-tools-version: 5.10\nimport PackageDescription\n'
            'let package = Package(name: "RemoteConsumer", platforms: [.macOS(.v14)],'
            'dependencies: [.package(path: "../release/bindings/swift/graph")],'
            'targets: [.executableTarget(name: "Consumer", dependencies: '
            '[.product(name: "ZeppelinEmbedGraph", package: "graph")])])\n')
        env = {k: v for k, v in os.environ.items() if not k.startswith('ZE_')}
        env['CLANG_MODULE_CACHE_PATH'] = str(work / 'clang-cache')
        run(['swift', 'build', '--package-path', consumer, '--scratch-path', work / 'build',
             '--cache-path', work / 'cache', '--jobs', '3'], env=env, cwd=work)
        result = run([work / 'build/debug/Consumer', work / 'fixture'], env=env, cwd=work,
                     capture_output=True, text=True)
        (output / 'swift.log').write_text(result.stdout + result.stderr)
        report['swift'] = dict(artifact_tools().measure(work / 'build/debug/Consumer', output),
                               reachability=consumer_receipt(result.stdout))
        report['reachability'] = {
            **{'c-' + kind: report['c'][key]['reachability']
               for kind, key in [('static', 'a'), ('dylib', 'dylib')]},
            'swift': report['swift']['reachability'], 'rust': report['rust']['reachability']}
        report['scope'] = 'remote installed batch/structured/get/resources/Cypher'
        (output / 'remote-report.json').write_text(json.dumps(report, indent=2) + '\n')
        require_structured_execution(report)


if __name__ == '__main__':
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--remote-tag', help='read-only post-publication proof against actual remote assets')
    parser.add_argument('--self-test', action='store_true')
    parser.add_argument('--profile-receipts', type=Path, help='ZE-74 installed C/Swift consumer receipts; source receipts do not qualify')
    parser.add_argument('--artifact-root', type=Path, default=ROOT / 'target')
    parser.add_argument('--output', type=Path, default=ROOT / 'tasks/evidence/ze-71-installed')
    parser.add_argument('--disable-swift-sandbox', action='store_true', help='explicit local workaround when nested sandbox-exec is unavailable')
    parser.add_argument('--legacy-xcframework', type=Path, help='explicit legacy zip for local qualification, still size-gated')
    parser.add_argument('--python-wheel', type=Path)
    parser.add_argument('--node-package', type=Path)
    parser.add_argument('--enforce-checksum', action='store_true', help='CI graph release bytes must match the separate graph pin')
    args = parser.parse_args()
    if args.self_test:
        unittest.main(argv=[__file__])
    elif args.remote_tag:
        remote_qualify(args)
    else:
        qualify(args)
