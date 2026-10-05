#!/usr/bin/env python3
"""Inspect explicit prebuilt legacy/graph-cypher artifacts; never invoke Cargo."""
import argparse
import hashlib
import importlib.util
import json
from pathlib import Path
import re
import subprocess
import tempfile

ROOT = Path(__file__).resolve().parents[2]


def load_packager():
    spec = importlib.util.spec_from_file_location('package_native', Path(__file__).with_name('package-native.py'))
    module = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(module)
    return module


def expected_symbols(artifact):
    if artifact not in ('legacy', 'graph-cypher'):
        raise ValueError(f'unknown artifact: {artifact}')
    # The legacy list never carries ze_graph_*; the graph-enabled archive's
    # complete export set lives in symbols.graph.allowlist.
    name = 'symbols.graph.allowlist' if artifact == 'graph-cypher' else 'symbols.allowlist'
    return {s for s in (ROOT / 'crates/zeppelin-embed-ffi' / name).read_text().splitlines()
            if s.startswith('ze_') and not s.startswith('ze_text_')
            and (artifact == 'graph-cypher' or not s.startswith('ze_graph_'))}


def check_exports(archive, artifact):
    result = subprocess.run([load_packager().llvm_tool('llvm-nm'), '--extern-only', '--defined-only', str(archive)], text=True, capture_output=True, check=True)
    raw = result.stdout
    observed = {s.lstrip('_') for s in re.findall(r'\b_?ze_\w+\b', raw)}
    expected = expected_symbols(artifact)
    if observed != expected:
        raise RuntimeError(f'{artifact} exports: missing={sorted(expected-observed)}, unexpected={sorted(observed-expected)}')


def measure(path, output):
    stripped = output / (path.name + '-stripped')
    subprocess.run(['cp', str(path), str(stripped)], check=True)
    subprocess.run(['strip', '-S', '-x', str(stripped)], check=True)
    raw = subprocess.check_output(['size', '-m', str(stripped)], text=True)
    (output / (path.name + '-size.txt')).write_text(raw)
    section_bytes = sum(int(m.group(1)) for m in re.finditer(r'^\s*Section (?!\(__LLVM,).*?\s(\d+)(?: \(zerofill\))?$', raw, re.M))
    if section_bytes == 0:
        raise RuntimeError(f'no linkable sections in {path}')
    return dict(section_bytes=section_bytes, section_kib=(section_bytes+1023)//1024,
                physical_bytes=path.stat().st_size,
                allocated_kib=int(subprocess.check_output(['du', '-k', str(path)], text=True).split()[0]),
                sha256=hashlib.sha256(path.read_bytes()).hexdigest(),
                stripped_physical_bytes=stripped.stat().st_size,
                stripped_allocated_kib=int(subprocess.check_output(['du', '-k', str(stripped)], text=True).split()[0]))


def check_artifact(legacy, graph, output):
    output.mkdir(parents=True, exist_ok=True)
    report = {}
    for identity, source in [('legacy', legacy), ('graph-cypher', graph)]:
        check_exports(source, identity)
        detail = output / identity
        detail.mkdir(exist_ok=True)
        trimmed = output / f'{identity}.a'
        load_packager().package('archive', source, trimmed)
        check_exports(trimmed, identity)
        report[identity] = dict(source=str(source.resolve()), original=measure(source, detail), trimmed=measure(trimmed, detail))
    consumer = output / 'graph-artifact-consumer'
    command = ['clang', '-std=c11', '-Wall', '-Wextra', '-Werror', '-mmacosx-version-min=14.0',
               '-I', str(ROOT / 'crates/zeppelin-embed-ffi/include'),
               str(ROOT / 'crates/zeppelin-embed-ffi/tests/c/graph_artifact_consumer.c'),
               str(output / 'graph-cypher.a'), '-framework', 'Security', '-liconv', '-o', str(consumer)]
    subprocess.run(command, check=True)
    with tempfile.TemporaryDirectory() as fixtures:
        subprocess.run([str(consumer.resolve()), fixtures], check=True)
    report['c_consumer'] = dict(command=command, **measure(consumer, output))
    negative = subprocess.run(['clang', '-x', 'c', '-', '-Wl,-force_load,' + str((output/'legacy.a').resolve()),
                              '-Wl,-force_load,' + str((output/'graph-cypher.a').resolve()),
                              '-framework', 'Security', '-liconv', '-o', str(output/'duplicate')],
                             input='int main(void) { return 0; }', text=True, capture_output=True)
    (output/'duplicate-link.txt').write_text(negative.stdout + negative.stderr)
    if negative.returncode == 0 or 'duplicate symbol' not in negative.stderr:
        raise RuntimeError('legacy plus graph must fail linking with duplicate symbols')
    report['duplicate_link'] = 'rejected: duplicate symbols'
    (output/'report.json').write_text(json.dumps(report, indent=2) + '\n')
    print(json.dumps(report, indent=2))
    return report


if __name__ == '__main__':
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--legacy', type=Path, required=True)
    parser.add_argument('--graph', type=Path, required=True)
    parser.add_argument('--output', type=Path, required=True)
    args = parser.parse_args()
    check_artifact(args.legacy, args.graph, args.output)
