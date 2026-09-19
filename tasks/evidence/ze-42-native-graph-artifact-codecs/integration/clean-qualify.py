from pathlib import Path
import hashlib, json, subprocess, os
os.environ['CARGO_LLVM_COV_TARGET_DIR'] = 'target/ze42-clean-coverage'

root = Path.cwd()
out = Path('/tmp/ze-42-integration')
baseline = json.loads((out / 'production-baseline.json').read_text())
changed = [p for p, h in baseline['files'].items() if hashlib.sha256((root / p).read_bytes()).hexdigest() != h]
expected_changed = ['crates/zeppelin-embed/src/format/mod.rs', 'crates/zeppelin-embed/src/property_graph/mod.rs', 'crates/zeppelin-embed/src/vfs/crash.rs', 'crates/zeppelin-embed/src/vfs/fault.rs', 'crates/zeppelin-embed/src/vfs/mod.rs', 'tests/adversarial-oracle/src/lib.rs']
assert sorted(changed) == expected_changed, changed
inventory = json.loads(Path('tasks/evidence/ze-42-native-graph-artifact-codecs/source-hashes.json').read_text())
merged = {'crates/zeppelin-embed/src/property_graph/mod.rs', 'tests/adversarial-oracle/src/lib.rs', 'tests/adversarial/runner.rs', 'tests/adversarial/mod.rs', 'tests/adversarial/coverage.rs', 'tests/adversarial_tests.rs'}
source_differences = [path for path, expected in inventory.items() if hashlib.sha256((root / path).read_bytes()).hexdigest() != expected]
root_tooling = json.loads((out / 'root-tooling-hashes.json').read_text())
assert all(hashlib.sha256((root / p).read_bytes()).hexdigest() == h for p, h in root_tooling.items())
assert set(source_differences) <= merged | set(root_tooling), source_differences
(out / 'source-audit.json').write_text(json.dumps({'base_commit': baseline['base_commit'], 'candidate_commit': subprocess.check_output(['git', 'rev-parse', 'HEAD'], text=True).strip(), 'modified_existing_production_files': changed, 'source_inventory': 'tasks/evidence/ze-42-native-graph-artifact-codecs/source-hashes.json', 'source_inventory_differences': source_differences, 'root_tooling_changes': root_tooling, 'other_source_hashes_match': True}, indent=2) + '\n')

commands = [
    ('clean-ffi-tests', ['cargo', 'llvm-cov', 'test', '--no-report', '-p', 'zeppelin-embed-ffi', '--features', 'abi-panic-probe,zeppelin-embed/test-support', '--test', 'ffi_boundary_validation']),
    ('clean-workspace-coverage', ['cargo', 'llvm-cov', 'report', '--fail-under-lines', '90', '--ignore-filename-regex', '(^|/)(registry/|crates/zeppelin-embed-bench|fuzz/|target/)', '--json', '--output-path', str(out / 'clean-workspace-coverage.json')]),
    ('clean-oracle-coverage', ['cargo', 'llvm-cov', 'report', '--no-default-ignore-filename-regex', '--ignore-filename-regex', '(^|/)(registry/|crates/zeppelin-embed-bench|fuzz/|target/|rustc/)|/\\.rustup/', '--json', '--output-path', str(out / 'clean-oracle-coverage.json')]),
]

results = []
for name, command in commands:
    print(f'Running {name}', flush=True)
    with (out / f'{name}.log').open('wb') as log:
        run = subprocess.run(command, stdout=log, stderr=subprocess.STDOUT)
    results.append({'name': name, 'command': command, 'exit_code': run.returncode})
    (out / 'clean-commands.json').write_text(json.dumps(results, indent=2) + '\n')
    if run.returncode:
        print((out / f'{name}.log').read_text()[-12000:], flush=True)
        raise SystemExit(run.returncode)
    print(f'{name}: exit 0', flush=True)

summary = {}
files = json.loads((out / 'clean-workspace-coverage.json').read_text())['data'][0]['files']
assert not any('zeppelin-embed-wt-' in f['filename'] for f in files)
for crate in ['zeppelin-embed', 'zeppelin-embed-cypher', 'zeppelin-embed-ffi', 'zeppelin-embed-text']:
    selected = [f for f in files if f'/crates/{crate}/src/' in f['filename']]
    covered = sum(f['summary']['lines']['covered'] for f in selected)
    count = sum(f['summary']['lines']['count'] for f in selected)
    assert selected and count
    summary[crate] = {'covered': covered, 'count': count, 'files': len(selected), 'percent': covered * 100 / count, 'passes_90': covered * 10 >= count * 9}
files = json.loads((out / 'clean-oracle-coverage.json').read_text())['data'][0]['files']
selected = [f for f in files if '/tests/adversarial-oracle/src/' in f['filename']]
covered = sum(f['summary']['lines']['covered'] for f in selected)
count = sum(f['summary']['lines']['count'] for f in selected)
assert selected and count
summary['zeppelin-embed-adversarial-oracle'] = {'covered': covered, 'count': count, 'files': len(selected), 'percent': covered * 100 / count, 'passes_90': covered * 10 >= count * 9}
(out / 'clean-per-crate.json').write_text(json.dumps(summary, indent=2) + '\n')
print(json.dumps(summary, indent=2), flush=True)
assert all(row['passes_90'] for row in summary.values()), 'Per-crate line coverage below 90%'
