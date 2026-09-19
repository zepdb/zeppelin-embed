from pathlib import Path
import datetime
import gzip
import json
import platform
import subprocess
import time

ROOT = Path('/Users/aghatage/Documents/code/zeppelin-embed')
OUT = Path('/tmp/ze-67-integration')
COMMANDS = [
    ('graph-contracts', ['cargo', 'nextest', 'run', '-p', 'zeppelin-embed-ffi', '--features', 'graph-cypher', '--test', 'ffi_graph_contract', '--test', 'ffi_graph_error', '--test', 'ffi_graph_header', '--test', 'ffi_graph_layout', '--test', 'ffi_contract', '--test', 'ffi_header']),
    ('legacy-contracts', ['cargo', 'nextest', 'run', '-p', 'zeppelin-embed-ffi', '--test', 'ffi_contract', '--test', 'ffi_header', '--test', 'ffi_graph_error', '--test', 'ffi_graph_header']),
    ('strict-ffi', ['cargo', 'clippy', '-p', 'zeppelin-embed-ffi', '--features', 'graph-cypher', '--lib', '--test', 'ffi_graph_contract', '--test', 'ffi_graph_error', '--test', 'ffi_graph_header', '--test', 'ffi_graph_layout', '--test', 'ffi_contract', '--test', 'ffi_header', '--no-deps', '--', '-D', 'warnings']),
    ('swift-error-enum', ['swiftc', '-typecheck', 'bindings/swift/Sources/ZeppelinEmbed/ZeppelinError.swift']),
    ('format', ['cargo', 'fmt', '--all', '--check']),
    ('diff', ['git', 'diff', '--check']),
]

results = []
for name, command in COMMANDS:
    start = time.monotonic()
    with (OUT / f'{name}.log').open('wb') as log:
        result = subprocess.run(command, cwd=ROOT, stdout=log, stderr=subprocess.STDOUT)
    raw = (OUT / f'{name}.log').read_bytes()
    (OUT / f'{name}.log.gz').write_bytes(gzip.compress(raw, mtime=0))
    results.append({'name': name, 'command': command, 'exit_code': result.returncode, 'elapsed_seconds': time.monotonic() - start})
    (OUT / 'commands.json').write_text(json.dumps({'cwd': str(ROOT), 'utc': datetime.datetime.now(datetime.timezone.utc).isoformat(), 'platform': platform.platform(), 'commands': results}, indent=2) + '\n')
    print(name, result.returncode, flush=True)
    if result.returncode:
        print(raw.decode(errors='replace')[-12000:], flush=True)
        raise SystemExit(result.returncode)
