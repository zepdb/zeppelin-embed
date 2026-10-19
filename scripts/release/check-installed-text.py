#!/usr/bin/env python3
"""Qualify installed C SDK and SwiftPM text consumers with build-tree reads denied."""
import argparse
import hashlib
import importlib.util
import json
import os
from pathlib import Path
import shutil
import subprocess
import tarfile
import tempfile

ROOT = Path(__file__).resolve().parents[2]
LINK_FLAGS = ['-lc++', '-liconv', '-framework', 'Security', '-framework', 'Foundation',
              '-framework', 'Metal', '-framework', 'CoreML', '-framework', 'Accelerate']
RECEIPT = 'ZE_TEXT_INSTALLED: open, ingest, dense query PASS'


def run(command, **kwargs):
    print('+', ' '.join(map(str, command)), flush=True)
    return subprocess.run(list(map(str, command)), **kwargs)


def build_swift_consumer(xcframework, work, disable_swift_sandbox=False):
    package = work / 'swiftpm'
    source = package / 'Sources/TextConsumer'
    source.mkdir(parents=True)
    shutil.copytree(xcframework, package / 'ZeppelinEmbedGraph.xcframework', symlinks=True)
    shutil.copyfile(Path(__file__).with_name('consumers') / 'text.swift', source / 'main.swift')
    shim = package / 'Sources/CZeppelinEmbed'
    shim.mkdir()
    shutil.copyfile(ROOT / 'bindings/swift/Sources/CZeppelinEmbedGraph/Exports.swift', shim / 'Exports.swift')
    (package / 'Package.swift').write_text('''// swift-tools-version: 5.10
import PackageDescription
let package = Package(name: "InstalledText", platforms: [.macOS(.v14)], targets: [
    .binaryTarget(name: "ZeppelinEmbedGraph", path: "ZeppelinEmbedGraph.xcframework"),
    .target(name: "CZeppelinEmbed", dependencies: ["ZeppelinEmbedGraph"]),
    .executableTarget(name: "TextConsumer", dependencies: ["CZeppelinEmbed"])
])
''')
    env = os.environ.copy()
    env['CLANG_MODULE_CACHE_PATH'] = str(work / 'clang-cache')
    env['SWIFTPM_MODULECACHE_OVERRIDE'] = str(work / 'swift-module-cache')
    command = ['swift', 'build', '-c', 'release', '--package-path', package,
               '--cache-path', work / 'swift-cache', '--jobs', '3']
    if disable_swift_sandbox:
        command.append('--disable-sandbox')
    run(command, cwd=work, env=env, check=True)
    binary = package / '.build/release/TextConsumer'
    resource = binary.parent / 'ZeppelinEmbedGraph.framework/Versions/A/Resources/mlx.metallib'
    if not binary.is_file() or not resource.is_file():
        raise FileNotFoundError('SwiftPM did not install the executable and framework metallib')
    return binary, resource


def run_isolated(flavor, binary, resource, fixture, output):
    # MLX has no environment override: deny the entire checkout so its absolute
    # build-tree metallib path cannot satisfy either installed-consumer run.
    profile = '(version 1)(allow default)(deny file-read* (subpath ' + json.dumps(str(ROOT.resolve())) + '))'
    env = {k: v for k, v in os.environ.items() if not k.startswith(('DYLD_', 'MLX_'))}
    report = dict(metallib_sha256=hashlib.sha256(resource.read_bytes()).hexdigest())
    for negative in (False, True):
        label = flavor + ('-missing-metallib' if negative else '')
        hidden = resource.with_name('mlx.metallib.withheld')
        if negative:
            resource.rename(hidden)
        try:
            result = run(['sandbox-exec', '-p', profile, binary,
                          binary.parent / ('store-negative' if negative else 'store-positive'), fixture],
                         cwd=binary.parent, env=env, capture_output=True, text=True)
        finally:
            if negative:
                hidden.rename(resource)
        (output / (label + '.log')).write_text(result.stdout + result.stderr)
        passed = (result.returncode != 0 and 'Failed to load the default metallib' in result.stdout + result.stderr
                  if negative else result.returncode == 0 and RECEIPT in result.stdout)
        report['negative' if negative else 'positive'] = dict(exit_code=result.returncode, passed=passed)
    return report


def qualify(artifact_root, output, disable_swift_sandbox=False):
    output = output.resolve()
    output.mkdir(parents=True, exist_ok=True)
    artifact_root = artifact_root.resolve()
    spec = importlib.util.spec_from_file_location('artifact', Path(__file__).with_name('check-graph-artifact.py'))
    tools = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(tools)
    report = {}
    try:
        with tempfile.TemporaryDirectory(prefix='ze506-installed-') as directory:
            work = Path(directory).resolve()
            if work.is_relative_to(ROOT):
                raise ValueError('installed consumers must be outside the repository')
            with tarfile.open(artifact_root / 'macos-sdk-graph-cypher/zeppelin-embed-graph-cypher-macos-arm64.tar.gz') as archive:
                archive.extractall(work, filter='data')
            # ditto preserves the versioned framework's symlinks.
            run(['ditto', '-x', '-k', artifact_root / 'xcframework-graph-cypher/ZeppelinEmbedGraph.xcframework.zip', work], check=True)
            sdk = work / 'zeppelin-embed-graph-cypher-macos-arm64'
            fixture = work / 'gte.zem'
            shutil.copyfile(ROOT / 'crates/zeppelin-embed-text/tests/fixtures/ze115/gte.zem', fixture)
            source = work / 'text.c'
            shutil.copyfile(Path(__file__).with_name('consumers') / 'text.c', source)
            builds = []
            for flavor, suffix in (('static', 'a'), ('dylib', 'dylib')):
                library = sdk / 'lib' / ('libzeppelin_embed_graph_cypher_ffi.' + suffix)
                resource = sdk / 'lib/mlx.metallib'
                for path in (sdk / 'include/zeppelin_embed.h', library, resource):
                    if not path.is_file() or path.stat().st_size == 0:
                        raise FileNotFoundError(f'missing installed text input: {path}')
                tools.check_exports(library, 'graph-cypher')
                install = work / flavor
                install.mkdir()
                installed_library = install / library.name
                shutil.copyfile(library, installed_library)
                shutil.copyfile(resource, install / 'mlx.metallib')
                binary = install / 'text-consumer'
                run(['clang', '-std=c11', '-Wall', '-Wextra', '-Werror', '-arch', 'arm64',
                     '-mmacosx-version-min=14.0', '-I', sdk / 'include', source,
                     installed_library, *LINK_FLAGS, '-Wl,-rpath,@executable_path', '-o', binary],
                    cwd=work, check=True)
                report[flavor] = dict(compile='PASS')
                builds.append((flavor, binary, install / 'mlx.metallib'))
            binary, resource = build_swift_consumer(work / 'ZeppelinEmbedGraph.xcframework', work, disable_swift_sandbox)
            report['swiftpm'] = dict(compile='PASS')
            builds.append(('swiftpm', binary, resource))
            for flavor, binary, resource in builds:
                report[flavor].update(run_isolated(flavor, binary, resource, fixture, output))
            if any(not row[k]['passed'] for row in report.values() for k in ('positive', 'negative')):
                raise RuntimeError(f'installed text qualification failed; see {output}/report.json')
    finally:
        (output / 'report.json').write_text(json.dumps(report, indent=2) + '\n')
    print('installed text: C static, C dylib, SwiftPM and missing-metallib controls PASS')
    return report


if __name__ == '__main__':
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--artifact-root', type=Path, default=ROOT / 'target')
    parser.add_argument('--output', type=Path, required=True)
    parser.add_argument('--disable-swift-sandbox', action='store_true',
                        help='disable only SwiftPM manifest sandboxing; runtime isolation remains mandatory')
    args = parser.parse_args()
    qualify(args.artifact_root, args.output, args.disable_swift_sandbox)
