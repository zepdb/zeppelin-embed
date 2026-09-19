from pathlib import Path
import hashlib, json, subprocess, os
os.environ['CARGO_LLVM_COV_TARGET_DIR'] = 'target/ze42-clean-coverage'
root = Path.cwd()
out = Path('/tmp/ze-35-integration')
baseline = json.loads((out / 'production-baseline.json').read_text())
changed = [p for p, h in baseline['files'].items() if hashlib.sha256((root / p).read_bytes()).hexdigest() != h]
assert sorted(changed) == ['crates/zeppelin-embed/src/property_graph/mod.rs', 'tests/adversarial-oracle/src/lib.rs'], changed
shared = {'crates/zeppelin-embed/CLAUDE.md', 'crates/zeppelin-embed/src/property_graph/mod.rs', 'tests/adversarial-oracle/src/lib.rs', 'tests/adversarial/runner.rs', 'tests/adversarial/mod.rs', 'tests/adversarial/coverage.rs', 'tests/adversarial_tests.rs', 'fuzz/Cargo.toml'}
manifest = json.loads((out / 'candidate-source-inventory.json').read_text())
differences = [r['path'] for r in manifest if hashlib.sha256((root/r['path']).read_bytes()).hexdigest() != r['sha256']]
assert set(differences) <= shared, differences
(out/'source-audit.json').write_text(json.dumps({'source_commit':'23fb6b961087281fbba90934966347d7231fa0aa','candidate_commit':subprocess.check_output(['git','rev-parse','HEAD'],text=True).strip(),'existing_production_changes':changed,'merged_inventory_differences':differences,'other_source_hashes_match':True,'environment':{'CARGO_LLVM_COV_TARGET_DIR':'target/ze42-clean-coverage'}},indent=2)+'\n')

commands = [
    ('core-tests', ['cargo', 'llvm-cov', 'test', '--no-report', '-p', 'zeppelin-embed', '--features', 'allocation-audit', '--lib', '--test', 'graph_catalog', '--', '--test-threads=1']),
    ('oracle-tests', ['cargo', 'llvm-cov', 'test', '--no-report', '-p', 'zeppelin-embed-adversarial-oracle', '--test', 'graph_catalog']),
    ('oracle-probe-tests', ['cargo', 'llvm-cov', 'test', '--no-report', '-p', 'zeppelin-embed-workspace-tests', '--test', 'adversarial_tests', 'property_graph', '--', '--nocapture']),
    ('combined-smoke', ['cargo', 'llvm-cov', 'test', '--no-report', '-p', 'zeppelin-embed-workspace-tests', '--test', 'adversarial_tests', 'smoke', '--', '--exact', '--nocapture']),
    ('clippy', ['cargo', 'clippy', '-p', 'zeppelin-embed', '-p', 'zeppelin-embed-adversarial-oracle', '-p', 'zeppelin-embed-workspace-tests', '--all-targets', '--features', 'zeppelin-embed/test-support,zeppelin-embed/allocation-audit', '--no-deps', '--', '-D', 'warnings']),
    ('fmt', ['cargo', 'fmt', '--all', '--check']),
    ('workspace-coverage', ['cargo', 'llvm-cov', 'report', '--fail-under-lines', '90', '--ignore-filename-regex', '(^|/)(registry/|crates/zeppelin-embed-bench|fuzz/|target/)', '--json', '--output-path', str(out / 'workspace-coverage.json')]),
    ('oracle-coverage', ['cargo', 'llvm-cov', 'report', '--no-default-ignore-filename-regex', '--ignore-filename-regex', '(^|/)(registry/|crates/zeppelin-embed-bench|fuzz/|target/|rustc/)|/\\.rustup/', '--json', '--output-path', str(out / 'oracle-coverage.json')]),
]
results = []
for name, command in commands:
    print(f'Running {name}', flush=True)
    with (out / f'{name}.log').open('wb') as log:
        run = subprocess.run(command, stdout=log, stderr=subprocess.STDOUT)
    results.append({'name': name, 'command': command, 'exit_code': run.returncode})
    (out / 'commands.json').write_text(json.dumps(results, indent=2) + '\n')
    if run.returncode:
        print((out / f'{name}.log').read_text()[-12000:], flush=True)
        raise SystemExit(run.returncode)
    print(f'{name}: exit 0', flush=True)

summary = {}
files = json.loads((out / 'workspace-coverage.json').read_text())['data'][0]['files']
assert not any('zeppelin-embed-wt-' in f['filename'] for f in files)
for crate in ['zeppelin-embed', 'zeppelin-embed-cypher', 'zeppelin-embed-ffi', 'zeppelin-embed-text']:
    selected = [f for f in files if f'/crates/{crate}/src/' in f['filename']]
    covered = sum(f['summary']['lines']['covered'] for f in selected)
    count = sum(f['summary']['lines']['count'] for f in selected)
    assert selected and count
    summary[crate] = {'covered': covered, 'count': count, 'files': len(selected), 'percent': covered * 100 / count, 'passes_90': covered * 10 >= count * 9}
files = json.loads((out / 'oracle-coverage.json').read_text())['data'][0]['files']
selected = [f for f in files if '/tests/adversarial-oracle/src/' in f['filename']]
covered = sum(f['summary']['lines']['covered'] for f in selected)
count = sum(f['summary']['lines']['count'] for f in selected)
assert selected and count
summary['zeppelin-embed-adversarial-oracle'] = {'covered': covered, 'count': count, 'files': len(selected), 'percent': covered * 100 / count, 'passes_90': covered * 10 >= count * 9}
(out / 'per-crate.json').write_text(json.dumps(summary, indent=2) + '\n')
print(json.dumps(summary, indent=2), flush=True)
assert all(row['passes_90'] for row in summary.values()), 'Per-crate line coverage below 90%'
