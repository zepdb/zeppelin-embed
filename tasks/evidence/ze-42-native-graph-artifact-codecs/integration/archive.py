from pathlib import Path
import gzip, hashlib, json

source = Path('/tmp/ze-42-integration')
destination = Path('tasks/evidence/ze-42-native-graph-artifact-codecs/integration')
result = json.loads((source / 'clean-workspace-result.json').read_text())
assert result['exit_code'] == 0
crates = json.loads((source / 'clean-per-crate.json').read_text())
assert all(row['passes_90'] for row in crates.values())
commands = json.loads((source / 'clean-commands.json').read_text())
assert commands and all(row['exit_code'] == 0 for row in commands)
inventory = json.loads((source / 'clean-source-inventory.json').read_text())
assert all(hashlib.sha256(Path(p).read_bytes()).hexdigest() == h for p, h in inventory['sources'].items())
destination.mkdir(parents=True, exist_ok=True)
small = ['source-audit.json', 'merge-audit.json', 'root-tooling-hashes.json', 'clean-source-inventory.json', 'clean-workspace-result.json', 'clean-commands.json', 'clean-per-crate.json', 'fuzz-checksum-repaired.json', 'fuzz-corpus.json', 'fuzz-root-independent-review.json', 'diagnostic-not-acceptance.json', 'denominator-audit.json', 'fresh-denominator-audit.json', 'profiles-before.json', 'preservation-audit.json', 'commands.json', 'qualify.py', 'clean-qualify.py', 'archive.py']
compressed = ['clean-workspace.log', 'clean-ffi-tests.log', 'clean-workspace-coverage.log', 'clean-workspace-coverage.json', 'clean-oracle-coverage.log', 'clean-oracle-coverage.json', 'fuzz-checksum-repaired.log', 'core-tests.log', 'oracle-probe-tests.log', 'runner-coverage.log', 'combined-smoke.log', 'clippy.log', 'workspace-coverage.json', 'oracle-coverage.json', 'workspace-coverage.log', 'oracle-coverage.log', 'per-crate.json']
manifest = []
for name in small + compressed:
    payload = (source / name).read_bytes()
    target = destination / (name + '.gz' if name in compressed else name)
    target.write_bytes(gzip.compress(payload, mtime=0) if name in compressed else payload)
    manifest.append({'source': name, 'retained': target.name, 'raw_sha256': hashlib.sha256(payload).hexdigest(), 'raw_bytes': len(payload), 'retained_sha256': hashlib.sha256(target.read_bytes()).hexdigest(), 'retained_bytes': target.stat().st_size})
(destination / 'manifest.json').write_text(json.dumps(manifest, indent=2) + '\n')
print('Retained', len(manifest), 'integration evidence files')
