#!/usr/bin/env python3
"""ZE-77 process supervisor. Standard library only. Never infers qualification."""
import argparse
import hashlib
import json
import os
from pathlib import Path
import shutil
import subprocess
import sys
import struct
import math
import tempfile
import time
import uuid

ROOT = Path(__file__).resolve().parents[1]
READS = ('project-evidence', 'semantic-context', 'alice-project-ranking',
         'lexical-evidence', 'hybrid-project-evidence', 'bounded-evidence')
EXTRA = ('exact', 'stress-10x', 'retention-1x', 'retention-5x', 'retention-10x',
         'paths-1', 'paths-2', 'paths-4', 'paths-8', 'paths-16', 'model-resident',
         'os-cold', 'recovery-64', 'recovery-16mib', 'mixed-4r1w',
         'structured-meeting-import', 'cypher-meeting-metadata')
PUBLIC_LEDGER_BLOCKER = 'exact work counters for C and Swift workers require a public ledger decision (ZE-76 chose internal)'
MISSING = {
    'ZE-313': 'Swift Cypher SIGBUS: crashing cells are failed cells, never passes',
    'ZE-310': 'application and canonical/fence retention observations remain reserved',
    'public-ledger-owner-decision': PUBLIC_LEDGER_BLOCKER,
    'model-owner-decision': 'owner must select model/runtime/precision and resident-memory observer',
    'os-cold-owner-decision': 'owner must approve and validate an OS cache-control method',
    'recovery-inputs': 'twenty digest-pinned observed tail stores per threshold with independent first-admission truth',
    'host-observer': 'quiet owner M3 Max and complete thermal/power/AC/worker-QoS/background CPU observer',
}
WORK_NAMES = ('operator_rows', 'adjacency_entries', 'expressions', 'hash_probes',
              'completed_rows', 'result_bytes', 'prepared_payload_bytes', 'completed_abi_bytes',
              'vector_coordinates', 'vector_payload_bytes', 'postings', 'lexical_blocks',
              'search_calls', 'directory_lookups', 'scans', 'paths', 'rows_in', 'rows_out',
              'join_probes', 'group_keys', 'eligibility_entries', 'copied_bytes',
              'directory_pages_decoded', 'directory_pages_copied', 'property_values',
              'property_bytes', 'adjacency_physical_entries', 'adjacency_merged_visits',
              'adjacency_merge_runs', 'eligibility_unique_entries', 'candidate_window_peak')
LEDGER_NAMES = ('storage_lookups', 'storage_scans', 'storage_adjacency_entries', 'storage_copied_bytes',
                'storage_pages_decoded', 'storage_pages_copied', 'storage_property_values', 'storage_property_bytes',
                'storage_adjacency_physical_entries', 'storage_adjacency_merged_visits', 'storage_adjacency_merge_runs',
                'canonical_comparison_bytes', 'canonical_encoding_bytes', 'wal_codec_units', 'encoded_wal_bytes',
                'artifact_bytes_written', 'artifact_writes', 'wal_bytes_appended', 'wal_appends',
                'full_sync_attempts', 'full_sync_successes', 'directory_sync_attempts', 'directory_sync_successes')

ABI_WORK_NAMES = dict(enumerate(WORK_NAMES[:22]))
ABI_WORK_NAMES[22] = 'query_reservation_peak_bytes'
RESOURCE_NAMES = ('engine_peak_bytes', 'engine_bytes', 'application_bytes', 'application_peak_bytes')


def require_counters(row, query=True, public_worker=False):
    counters = row.get('counters', {})
    for key in RESOURCE_NAMES + (WORK_NAMES if query or public_worker else ()):
        value = counters.get(key)
        if type(value) is not int or value < 0:
            if public_worker and key not in RESOURCE_NAMES:
                raise ValueError(f'public-ledger-owner-decision: {PUBLIC_LEDGER_BLOCKER}; missing/invalid {key}')
            raise ValueError(f'missing/invalid ZE-76 counter {key}')
    return counters


def public_counters(row):
    """Only public resources and response work kinds 0..22; never fill gaps."""
    counters = dict(row['resources'])
    work = row['work_raw']
    if 'global_work' in row:
        span = row['global_work']; start, count = span['start'], span['count']
        if start < 0 or count < 0 or start + count > len(work): raise ValueError('invalid global work range')
        work = work[start:start+count]
    for entry in work:
        kind = entry['kind']
        if type(kind) is not int or kind not in ABI_WORK_NAMES: raise ValueError('unknown public work kind')
        name = ABI_WORK_NAMES[kind]
        if name in counters: raise ValueError('duplicate global work counter')
        counters[name] = entry['value']
    require_counters({'counters': counters}, query=False)
    return counters


def smoke_manifest(fixtures):
    manifest = accepted_manifest(fixtures)
    manifest.update(smoke=True, repetitions=1, warmups=5, samples=50,
                    cells=['baseline/A/rust/structured/project-evidence'],
                    qualification='SMOKE ONLY; rejected by acceptance validator')
    manifest['cell_protocols'] = {manifest['cells'][0]: {'warmups': 5, 'samples': 50}}
    return manifest



def run(argv, **kw):
    return subprocess.run([str(v) for v in argv], cwd=ROOT, check=True, **kw)


def digest(path):
    h = hashlib.sha256()
    with Path(path).open('rb') as f:
        for block in iter(lambda: f.read(1024 * 1024), b''):
            h.update(block)
    return h.hexdigest()


def copy_verified(source, dest, expected):
    """Exclusive streamed copy, hash both source and completed destination."""
    source, dest = Path(source), Path(dest)
    start = time.monotonic_ns()
    h = hashlib.sha256()
    try:
        with source.open('rb') as src, dest.open('xb') as dst:
            for block in iter(lambda: src.read(1024 * 1024), b''):
                h.update(block)
                dst.write(block)
            dst.flush()
            os.fsync(dst.fileno())
        if h.hexdigest() != expected or digest(source) != expected or digest(dest) != expected:
            raise ValueError(f'digest drift: {source}')
    except BaseException:
        # This invocation exclusively created dest. Never remove another file.
        if 'dst' in locals():
            dest.unlink(missing_ok=True)
        raise
    return {'method': 'streamed-1MiB-sha256-fsync', 'copy_ns': time.monotonic_ns() - start,
            'bytes': source.stat().st_size, 'sha256': expected}


def copy_fixture(source, dest):
    source, dest = Path(source).resolve(), Path(dest)
    expected_manifest_hash = digest(source / 'manifest.json')
    manifest = json.loads((source / 'manifest.json').read_text())
    if digest(source / 'manifest.json') != expected_manifest_hash: raise ValueError('fixture manifest drift')
    dest.mkdir(parents=True, exist_ok=False)
    records = []
    for f in manifest['files']:
        name = f['name']
        if Path(name).name != name:
            raise ValueError('unsafe fixture filename')
        records.append(copy_verified(source / name, dest / name, f['sha256']))
        if (dest / name).stat().st_size != f['bytes']:
            raise ValueError('fixture size differs')
    records.append(copy_verified(source / 'manifest.json', dest / 'manifest.json',
                                 expected_manifest_hash))
    return records


def build_workers(output, release=False):
    output = Path(output).resolve()
    output.mkdir(parents=True, exist_ok=True)
    env = dict(os.environ, CARGO_BUILD_JOBS='3')
    cmd = ['cargo', 'build', '-p', 'zeppelin-embed-workspace-tests',
           '--features', 'graph-cypher', '--bin', 'graph-workload', '--bin', 'graph-fixture']
    if release:
        cmd.append('--release')
    run(cmd, env=env)
    profile = 'release' if release else 'debug'
    for name in ('graph-workload', 'graph-fixture'):
        shutil.copy2(ROOT / 'target' / profile / name, output / name)
    run(['cargo', 'build', '-p', 'zeppelin-embed-ffi', '--features', 'graph-cypher'] +
        (['--release'] if release else []), env=env)
    archive = ROOT / 'target' / profile / 'libzeppelin_embed_ffi.a'
    run(['cc', '-std=c11', '-Wall', '-Wextra', '-Werror', '-O3',
         '-I', ROOT / 'crates/zeppelin-embed-ffi/include',
         ROOT / 'crates/zeppelin-embed-ffi/tests/c/graph_workload.c', archive,
         '-framework', 'Security', '-framework', 'CoreFoundation', '-lpthread', '-ldl',
         '-o', output / 'graph-workload-c'])
    # Link the public typed Swift wrapper against this build.
    env.update(ZE_USE_LOCAL_FFI='1', ZE_LOCAL_FFI_ARCHIVE=str(archive), SWIFT_MODULECACHE_PATH=str(output / 'swift-cache'), CLANG_MODULE_CACHE_PATH=str(output / 'swift-cache'))
    run(['swift', 'package', '--package-path', ROOT / 'bindings/swift/graph', 'clean'], env=env)
    run(['swift', 'build', '--package-path', ROOT / 'bindings/swift/graph',
         '--product', 'GraphWorkload', '--disable-sandbox', '-j', '3'] +
        (['-c', 'release'] if release else []), env=env)
    bindir = run(['swift', 'build', '--package-path', ROOT / 'bindings/swift/graph',
                  '--show-bin-path'] + (['-c', 'release'] if release else []),
                 env=env, capture_output=True, text=True).stdout.strip()
    shutil.copy2(Path(bindir) / 'GraphWorkload', output / 'graph-workload-swift')
    metadata = {'profile': profile, 'source': run(['git', 'rev-parse', 'HEAD'],
                capture_output=True, text=True).stdout.strip(),
                'dirty': run(['git', 'status', '--porcelain'], capture_output=True, text=True).stdout,
                'rustc': run(['rustc', '--version'], capture_output=True, text=True).stdout.strip(),
                'archive_sha256': digest(archive), 'header_sha256': digest(ROOT / 'crates/zeppelin-embed-ffi/include/zeppelin_graph_contracts.h'),
                'workers': {p.name: digest(p) for p in output.glob('graph-workload*')}}
    (output / 'build.json').write_text(json.dumps(metadata, indent=2) + '\n')


def accepted_manifest(fixtures):
    return {'version': 1, 'repetitions': 5, 'warmups': 50, 'samples': 1000,
            'imports': 200, 'reader_requests': 1000, 'writer_requests': 200,
            'recovery_opens': 20, 'recall_k': 20, 'recall_floor': 0.95,
            'absolute_tolerance': 1e-6, 'relative_tolerance': 1e-6,
            'p95_target_ns': 250_000_000, 'managed_peak_limit_bytes': 256 << 20,
            'seed': 'fixture-v1/ZE_TEST_SEED', 'fixtures': fixtures,
            'cells': [f'baseline/{st}/{lang}/{front}/{name}' for st in ('A', 'B')
                      for lang in ('rust', 'c', 'swift') for front in ('structured', 'cypher')
                      for name in READS] +
                     [f'{experiment}/{st}/{lang}/{front}/{name}' for experiment in ('exact','stress-10x')
                      for st in ('A','B') for lang in ('rust','c','swift') for front in ('structured','cypher') for name in READS] +
                     [f'paths-{hop}/{st}/{lang}/{front}/bounded-evidence' for hop in (1,2,4,8,16)
                      for st in ('A','B') for lang in ('rust','c','swift') for front in ('structured','cypher')] +
                     [f'{experiment}/{st}/{lang}' for experiment in ('structured-meeting-import','cypher-meeting-metadata')
                      for st in ('A','B') for lang in ('rust','c','swift')] +
                     [f'mixed-4r1w/{st}/{lang}' for st in ('A','B') for lang in ('rust','c','swift')] +
                     [e for e in EXTRA if e not in ('exact','stress-10x','structured-meeting-import','cypher-meeting-metadata','mixed-4r1w') and not e.startswith('paths-')],
            'threshold_failure_is_evidence': True,
            'blocked_inputs': MISSING,
            'model': None, 'cache_control': None,
            'qualification': 'blocked; tooling preparation only'}


def prepare(fixtures, manifest_path, workers, smoke=False):
    if not smoke and not fixtures: raise ValueError('acceptance preparation requires explicit baseline and stress fixtures')
    inventory = []
    base = Path(manifest_path).resolve().parent
    base.mkdir(parents=True, exist_ok=True)
    failures = []
    if smoke and not fixtures:
        generated = base / 'small-input'
        run([workers / 'graph-fixture', 'generate', 'small', generated,
             run(['git', 'rev-parse', 'HEAD'], capture_output=True, text=True).stdout.strip()])
        fixtures = [generated]
    for index, source in enumerate(fixtures):
        source = Path(source).resolve()
        raw = json.loads((source / 'manifest.json').read_text())
        destination = base / f'fixture-{index}'
        copies = copy_fixture(source, destination)
        run([workers / 'graph-fixture', 'validate', destination])
        inventory.append({'path': str(destination), 'scale': 'baseline' if smoke else raw['scale'], 'fixture_scale': raw['scale'],
                          'manifest_sha256': digest(destination / 'manifest.json'), 'copies': copies})
        for state in (('A',) if smoke else ('A', 'B')):
            receipts = base / f'receipts-{index}-{state}.jsonl'
            store = base / f'store-{index}-{state}'
            command = [workers / 'graph-workload', 'ingest', destination, store, state, receipts]
            completed = subprocess.run([str(x) for x in command], cwd=ROOT, capture_output=True, text=True)
            (base / f'ingest-{index}-{state}.log').write_text(completed.stdout + completed.stderr)
            if completed.returncode:
                failures.append({'fixture': index, 'state': state, 'exit': completed.returncode,
                                 'error': completed.stderr, 'command': [str(x) for x in command]})
                continue
            truth = base / f'truth-{index}-{state}.jsonl'
            run([workers / 'graph-workload', 'truth-smoke' if smoke else 'truth', destination, state, receipts, truth])
            imports = base / f'imports-{index}-{state}'
            if not smoke: import_jobs(destination, receipts, imports)
            inventory[-1][state] = {'imports': str(imports), 'store': str(store), 'receipts': str(receipts),
                                    'truth': str(truth), 'truth_sha256': digest(truth)}
    manifest = smoke_manifest(inventory) if smoke else accepted_manifest(inventory)
    manifest['preparation_failures'] = failures
    if not smoke:
        manifest['cell_protocols'] = {cell: {'warmups': 0 if cell.startswith(('structured-meeting-import','cypher-meeting-metadata','mixed-4r1w','recovery-','retention-')) else 50,
                                       'samples': 4200 if cell.startswith('mixed-4r1w') else 200 if 'meeting' in cell else 20 if cell.startswith('recovery-') else 1000,
                                       'target_p95_ns': 250_000_000 if cell.startswith(('baseline/','structured-meeting-import/','cypher-meeting-metadata/')) else 2_000_000_000 if cell.startswith('recovery-') else None} for cell in manifest['cells']}
        baseline = next((f for f in inventory if f['scale'] == 'baseline'), None)
        if baseline:
            counts = json.loads((Path(baseline['path']) / 'manifest.json').read_text())['inventory']
            keys = counts['nodes'] + counts['edges']
            for multiple in (1, 5, 10):
                manifest['cell_protocols'][f'retention-{multiple}x']['samples'] = (keys * multiple + 127) // 128
    if failures:
        manifest['blocked_inputs']['preparation'] = 'actual ingestion failures; see retained logs'
    with Path(manifest_path).open('x') as f:
        json.dump(manifest, f, indent=2)
        f.write('\n')
    if failures:
        raise ValueError('preparation failed; retained all receipts/logs. No baseline readiness; ZE-290 handoff still required')


def import_jobs(fixture, receipts, output, count=200):
    """Freeze supplied-payload and graph-only meeting requests outside timing."""
    ids = {}
    for line in Path(receipts).open():
        for r in json.loads(line).get('receipts', []):
            ids[(r['kind'], r['key']['namespace'], r['key']['key'])] = int(r['id'])
    template = None
    with (Path(fixture) / 'batches-a.jsonl').open() as f:
        for line in f:
            row = json.loads(line)
            if len(row['changes']) == 137:
                template = row['changes']; break
    if template is None:
        raise ValueError('missing 27-node/110-relationship meeting template')
    output = Path(output); output.mkdir(exist_ok=False)
    for iteration in range(count):
        changes = json.loads(json.dumps(template))
        local = {(c['image']['key']['namespace'], c['image']['key']['key']): index
                 for index, c in enumerate(changes) if c['image']['kind'] == 'node'}
        for c in changes:
            image = c['image']; k = image['key']; k['key'] = f'ze77-import-{iteration}-' + k['key']
            if image['kind'] == 'node' and image['vector'] is not None:
                ref = image['vector']
                with (Path(fixture) / ref['file']).open('rb') as f:
                    f.seek(ref['offset']); data = f.read(768 * 4)
                    if len(data) != 768 * 4: raise ValueError('truncated supplied vector')
                    image['vector_bits'] = list(struct.unpack('<768I', data))
            if image['kind'] == 'relationship':
                for name in ('source', 'target'):
                    k = image[name]; token = (k['namespace'], k['key'])
                    if token in local: k['key'] = f'ze77-import-{iteration}-' + k['key']
                    else:
                        ident = ids[('node', *token)]
                        k.update(high=str(ident >> 64), low=str(ident & ((1 << 64) - 1)))
        (output / f'{iteration}.batch.json').write_text(json.dumps(changes))
        write_c_batch(changes, output / f'{iteration}.batch')
        source = meeting_metadata(changes)
        if len(source.encode()) > 65536: raise ValueError('metadata source exceeds accepted guard')
        (output / f'{iteration}.cypher').write_text(source)
    for frontend, suffix in (('c-batch', 'batch'), ('swift-batch', 'batch.json'), ('cypher', 'cypher')):
        (output / (frontend + '.list')).write_text(''.join(str((output / f'{i}.{suffix}').resolve()) + '\n' for i in range(count)))


def meeting_metadata(changes):
    nodes = [(i, c['image']) for i, c in enumerate(changes) if c['image']['kind'] == 'node']
    local = {(n['key']['namespace'], n['key']['key']): f'n{i}' for i, n in nodes}
    external = {}
    for c in changes:
        if c['image']['kind'] == 'relationship':
            for name in ('source', 'target'):
                k = c['image'][name]; token = (k['namespace'], k['key'])
                if token not in local: external[token] = k
    parts = []
    for i, (token, k) in enumerate(sorted(external.items())):
        alias = f'e{i}'; local[token] = alias
        ident = (int(k['high']) << 64) | int(k['low'])
        parts.append(f"MATCH ({alias}) WHERE ze.node_id({alias}) = '{ident:032x}'")
    def properties(raw):
        def literal(v):
            if 'string' in v: return "'" + v['string'].replace('\\', '\\\\').replace("'", "\\'") + "'"
            if 'i64' in v: return str(v['i64'])
            if 'bool' in v: return str(v['bool']).lower()
            if 'f64_bits' in v: return str(struct.unpack('<d', struct.pack('<Q', int(v['f64_bits'], 16)))[0])
            if 'string_list' in v: return '[' + ','.join(literal({'string': x}) for x in v['string_list']) + ']'
            raise ValueError('unknown primitive property')
        return '{' + ','.join(k + ':' + literal(v) for k, v in sorted(raw.items())) + '}'
    for i, n in nodes:
        parts.append(f"CREATE (n{i}:" + ':'.join(n['labels']) + ' ' + properties(n['properties']) + ')')
    for i, c in enumerate(changes):
        r = c['image']
        if r['kind'] != 'relationship': continue
        a = local[(r['source']['namespace'], r['source']['key'])]
        b = local[(r['target']['namespace'], r['target']['key'])]
        parts.append(f"CREATE ({a})-[r{i}:{r['type']} " + properties(r['properties']) + f']-({b})')
        parts[-1] = parts[-1].replace(']-(', ']->(')
    parts.append('RETURN 0 LIMIT 0')
    return ' '.join(parts)


def write_c_batch(changes, path):
    data = bytearray(); records = []; values = []; names = []; children = []; properties = []; nodes = []; rels = []; vectors = []; items = []
    def span(text):
        raw = text.encode(); start = len(data); data.extend(raw); return (start, len(raw))
    def scalar(v):
        if 'string' in v: r = span(v['string']); row = (4, 0, 0, 0, 0, *r)
        elif 'i64' in v: row = (2, 0, 0, v['i64'], 0, 0, 0)
        elif 'bool' in v: row = (1, 0, int(v['bool']), 0, 0, 0, 0)
        elif 'f64_bits' in v: row = (3, 0, 0, 0, int(v['f64_bits'], 16), 0, 0)
        elif 'string_list' in v:
            start = len(children)
            indices = [scalar({'string': x}) for x in v['string_list']]
            children.extend(indices); row = (7, 4, 0, 0, 0, start, len(indices))
        else: raise ValueError('unsupported batch scalar')
        index = len(values); values.append(row); return index
    local = {(c['image']['key']['namespace'], c['image']['key']['key']): i
             for i, c in enumerate(changes) if c['image']['kind'] == 'node'}
    for c in changes:
        image = c['image']; ps = len(properties)
        for k, v in sorted(image['properties'].items()): properties.append((*span(k), scalar(v)))
        pr = (ps, len(properties) - ps)
        if image['kind'] == 'node':
            label_start = len(names); names.extend(span(s) for s in image['labels']); text = image['text']; tr = span(text) if text is not None else (0, 0)
            vs = len(vectors); vectors.extend(image.get('vector_bits', [])); vr = (vs, len(vectors) - vs) if image.get('vector_bits') is not None else (0, 0)
            index = len(nodes); nodes.append((int(text is not None), int('vector_bits' in image), *pr, *tr, *vr, label_start, len(names) - label_start)); kind = 0; ends = [0] * 8
        else:
            index = len(rels); rels.append((*pr, *span(image['type']))); kind = 1; ends = []
            for name in ('source', 'target'):
                k = image[name]; token = (k['namespace'], k['key'])
                ends.extend((2, local[token], 0, 0) if token in local else (1, 0, int(k['high']), int(k['low'])))
        k = image['key']; items.append((kind, *span(k['namespace']), *span(k['key']), index, *ends))
    if len(nodes) != 27 or len(rels) != 110 or len(vectors) != 20 * 768:
        raise ValueError('meeting dimensions differ')
    with Path(path).open('x') as f:
        f.write('ZE77JOB1\nB ' + data.hex() + '\n')
        for tag, rows in (('W', values), ('N', names), ('Q', properties), ('D', nodes), ('L', rels), ('A', items)):
            for row in rows: f.write(tag + ' ' + ' '.join(str(x) for x in row) + '\n')
        for tag, rows in (('F', vectors), ('C', children)):
            for x in rows: f.write(f'{tag} {x}\n')


def monitor_repetition(command, output, observer, timeout, provenance_paths=()):
    """Fresh worker plus independent 1-Hz observer. Never invent OS readings.

    Observer is a persistent process taking worker PID as its last argument;
    stdout is JSONL actual thermal/power/AC/QoS/background CPU observations.
    Missing/invalid observations are rejected by the shared Rust validator.
    """
    output = Path(output)
    output.mkdir(parents=True, exist_ok=False)
    provenance = {str(Path(x).resolve()): digest(x) for x in list(command) + list(provenance_paths) if Path(str(x)).is_file()}
    started = time.monotonic_ns()
    with (output / 'samples.jsonl').open('w') as stdout, (output / 'worker.stderr').open('w') as stderr:
        worker = subprocess.Popen([str(x) for x in command], stdout=stdout, stderr=stderr, cwd=ROOT)
        with (output / 'monitor.jsonl').open('w') as monitor, (output / 'monitor.stderr').open('w') as errors:
            try:
                watching = subprocess.Popen(observer + [str(worker.pid)], stdout=monitor, stderr=errors, cwd=ROOT)
            except BaseException:
                worker.kill(); worker.wait(); raise
            try:
                status = worker.wait(timeout=timeout)
            except subprocess.TimeoutExpired:
                worker.kill()
                worker.wait()
                status = -1
            finally:
                watching.terminate()
                watching.wait(timeout=10)
    (output / 'process.json').write_text(json.dumps({'pid': worker.pid,
       'process_nonce': str(uuid.uuid4()), 'duration_ms': (time.monotonic_ns() - started) // 1_000_000,
       'digests_before': provenance, 'digests_after': {p: digest(p) for p in provenance},
       'exit': status, 'timeout_is_failure': status == -1, 'command': [str(x) for x in command]}, indent=2))
    if status:
        (output / 'repetition.json').write_text(json.dumps({'correct': False,
            'error': f'worker exit {status}', 'command': [str(x) for x in command],
            'blocker': 'ZE-313' if 'swift' in str(command[0]) and status == -10 else None}, indent=2))
        raise ValueError(f'failed cell retained at {output}; exit={status}')


def run_matrix(args):
    manifest = json.loads(Path(args.manifest).read_text())
    # Deliberately no date/time shortcut for either missing inputs or quiet host.
    missing = manifest.get('blocked_inputs', {})
    if missing and not args.smoke:
        raise ValueError('authoritative measurement blocked: ' + '; '.join(f'{k}: {v}' for k, v in missing.items()))
    if manifest.get('preparation_failures'):
        raise ValueError('failed fixture preparation; cannot measure partial store')
    if bool(manifest.get('smoke')) != args.smoke:
        raise ValueError('--smoke must match the explicitly marked manifest')
    if not args.observer:
        if not args.smoke:
            raise ValueError('owner M3 Max requires complete actual host observer')
        args.observer = [str(Path(args.workers).resolve() / 'graph-workload-swift'), 'observe']
    build = json.loads((Path(args.workers) / 'build.json').read_text())
    if build['profile'] != 'release' and not args.smoke:
        raise ValueError('authoritative cells require release opt-level=3 workers')
    for cell in manifest['cells']:
        for repetition in range(manifest['repetitions']):
            directory = Path(args.output) / cell / str(repetition)
            directory.mkdir(parents=True, exist_ok=False)
            accepted = None
            for attempt in range(args.max_attempts):
                attempt_dir = directory / ('attempt-' + str(attempt))
                attempt_dir.mkdir(parents=True, exist_ok=False)
                provenance = [Path(args.manifest), Path(args.workers) / 'build.json']
                for f in manifest['fixtures']:
                    provenance.extend(p for p in Path(f['path']).iterdir() if p.is_file())
                    for st in ('A','B'):
                        if st in f:
                            provenance.extend(p for p in Path(f[st]['truth']).with_suffix('.jobs').iterdir() if p.is_file())
                try:
                    command = cell_command(manifest, cell, Path(args.workers).resolve(), attempt_dir)
                    monitor_repetition(command, attempt_dir / 'worker', args.observer, args.timeout, provenance)
                    record = normalize_repetition(manifest, cell, repetition, attempt_dir / 'worker', command)
                except (ValueError, OSError, subprocess.SubprocessError) as error:
                    failure = dict(cell=cell, repetition=repetition, correct=False, error=str(error),
                                   blocker='ZE-313' if '/swift/cypher/' in cell or cell.startswith('cypher-meeting-metadata/') and cell.endswith('/swift') else None)
                    (attempt_dir / 'failed-cell.json').write_text(json.dumps(failure, indent=2) + '\n')
                    with (Path(args.output) / 'repetitions.jsonl').open('a') as output:
                        output.write(json.dumps(failure) + '\n')
                    raise
                finally:
                    # Keep receipts, samples, hashes and oracle; never accumulate store copies.
                    if (attempt_dir / 'store').exists(): shutil.rmtree(attempt_dir / 'store')
                    for tail in attempt_dir.glob('tail-*'):
                        if tail.is_dir(): shutil.rmtree(tail)
                with (Path(args.output) / 'repetitions.jsonl').open('a') as output:
                    output.write(json.dumps(record) + '\n')
                if args.smoke or not record['tainted']:
                    accepted = record; break
            if accepted is None: raise ValueError('whole repetitions remain tainted; all attempts retained, qualification blocked')
    write_report(args)




def cell_command(manifest, cell, workers, directory):
    parts = cell.split('/')
    scale = 'stress-10x' if cell.startswith('stress-10x') else 'baseline'
    fixture = next((f for f in manifest['fixtures'] if f['scale'] == scale), None)
    if fixture is None: raise ValueError(f'missing digest-checked {scale} fixture')
    state = parts[1] if len(parts) > 1 and parts[1] in ('A', 'B') else 'A'
    inputs = fixture.get(state)
    if inputs is None: raise ValueError(f'missing complete {scale}/{state} preparation; ZE-290 uninterrupted ingestion handoff required')
    if parts[0] == 'model-resident': raise ValueError('owner decision required: select model/runtime/precision and resident-memory observer')
    if parts[0] == 'os-cold': raise ValueError('owner decision required: approve and validate OS cache-control method')
    store = directory / 'store'
    if not parts[0].startswith('recovery-'):
        copies = copy_store(inputs['store'], store)
        (directory / 'copies.json').write_text(json.dumps(copies, indent=2))
    jobs = Path(inputs['truth']).with_suffix('.jobs')
    if parts[0] in ('baseline', 'exact', 'stress-10x') or parts[0].startswith('paths-'):
        if len(parts) != 5: raise ValueError('read cell must declare state/language/frontend/name')
        _, state, language, frontend, name = parts
        exact = parts[0] == 'exact'
        if parts[0].startswith('paths-'):
            name += '-h' + parts[0].split('-')[1]
        protocol = manifest.get('cell_protocols', {}).get(cell, manifest)
        warmups, samples = protocol['warmups'], protocol['samples']
        if language == 'rust':
            return [workers / 'graph-workload', 'read-schedule', store, jobs, name,
                    frontend, 'exact' if exact else 'auto', str(warmups), str(samples)]
        suffix = ('.exact' if exact else '') + '.' + frontend
        paths = [str((jobs / (str(i) + '-' + name + suffix)).resolve()) for i in range(100)]
        schedule = directory / 'schedule.list'; schedule.write_text('\n'.join(paths) + '\n')
        return [workers / ('graph-workload-' + language), store, frontend, '@' + str(schedule), str(warmups), str(samples)]
    if parts[0] in ('structured-meeting-import', 'cypher-meeting-metadata'):
        if len(parts) != 3: raise ValueError('import cell must declare state/language')
        language = parts[2]
        if language == 'rust' and parts[0] == 'structured-meeting-import':
            return [workers / 'graph-workload', 'imports', store, fixture['path'], inputs['receipts'], '200']
        jobs = Path(inputs['imports'])
        if language == 'rust':
            return [workers / 'graph-workload', 'cypher-imports', store, jobs / 'cypher.list', '200']
        front = 'cypher' if parts[0] == 'cypher-meeting-metadata' else 'batch'
        list_name = 'cypher.list' if front == 'cypher' else language + '-batch.list'
        return [workers / ('graph-workload-' + language), store, front, '@' + str(jobs / list_name), '0', '200']
    if parts[0] == 'mixed-4r1w':
        language = parts[2] if len(parts) > 2 else 'rust'
        if language != 'rust':
            suffix = 'structured'
            schedule = directory / 'reads.list'
            schedule.write_text(''.join(str((jobs / (str(i) + '-alice-project-ranking.' + suffix)).resolve()) + '\n' for i in range(100)))
            imports = Path(inputs['imports']) / (language + '-batch.list')
            return [workers / ('graph-workload-' + language), 'mixed', store, schedule, imports]
        return [workers / 'graph-workload', 'mixed', store, fixture['path'], inputs['receipts'], jobs]
    if parts[0].startswith('retention-'):
        inventory = json.loads((Path(fixture['path']) / 'manifest.json').read_text())['inventory']
        keys = inventory['nodes'] + inventory['edges']
        multiple = parts[0].split('-')[1].rstrip('x')
        return [workers / 'graph-workload', 'retention', store, str(keys), multiple]
    if parts[0].startswith('recovery-'):
        # ZE-76 must authenticate actual tail boundaries; requested write count
        # is not proof of observed WAL/checkpoint state.
        tails = manifest.get('observed_tail_stores', {}).get(parts[0])
        if not tails or len(tails) != 20:
            raise ValueError('missing ZE-76: twenty digest-pinned observed 64-envelope/16MiB bounded-tail stores through first admission')
        paths = []
        for index, tail in enumerate(tails):
            capture = tail.get('capture', {})
            expected = capture.get('envelopes') == 64 if parts[0] == 'recovery-64' else capture.get('wal_bytes') == 16 * 1024 * 1024
            if 'truth_rows' not in tail or 'truth_generation' not in tail: raise ValueError('missing independent tail admission truth')
            if not expected or not capture.get('checkpoint_boundary_observed') or 'creation_serial_count' not in capture or 'full_syncs' not in capture or 'directory_syncs' not in capture:
                raise ValueError('missing ZE-76 observed tail-boundary/creation-serial/sync capture; requested envelope count is not proof')
            target = directory / ('tail-' + str(index))
            copies = copy_store(tail['path'], target)
            (directory / ('tail-' + str(index) + '-copy.json')).write_text(json.dumps(copies, indent=2))
            paths.append(str(target.resolve()))
        listing = directory / 'tails.list'; listing.write_text('\n'.join(paths) + '\n')
        return [workers / 'graph-workload', 'recovery-series', listing, 'read-write']
    raise ValueError(f'undeclared execution protocol {cell}')


def oracle_check(expected, observed, generation):
    # Entity payload checks encode the complete typed description in one cell.
    # JSON preserves f32/f64 bit strings, null vs empty, endpoints and list order.
    e = [[{'string': json.dumps(v, sort_keys=True, separators=(',', ':'))}] for v in expected]
    o = [[{'string': json.dumps(v, sort_keys=True, separators=(',', ':'))}] for v in observed]
    compare_rows(e, o)
    return dict(expected_rows=e, observed_rows=o, admitted_generation=generation,
                truth_generation=generation, truth_ids=[], hit_ids=[], correct=True)


def inspect_entities(verifier, store, entities, directory):
    requests = directory / 'entity-requests.jsonl'
    requests.write_text(''.join(json.dumps({'kind': k, 'id': str(i)}) + '\n' for k, i in entities))
    result = subprocess.run([str(verifier), 'inspect-entities', str(store), str(requests)],
                            capture_output=True, text=True, cwd=ROOT)
    (directory / 'entity-observations.jsonl').write_text(result.stdout)
    (directory / 'entity-observations.stderr').write_text(result.stderr)
    if result.returncode: raise ValueError('complete public entity observation failed: ' + result.stderr)
    observed = [json.loads(line)['observed'] for line in result.stdout.splitlines()]
    if len(observed) != len(entities): raise ValueError('partial public entity observations')
    return observed


def normalize_imports(measured, fixture, inputs, verifier, store, directory, metadata=False):
    baseline = {}
    for line in Path(inputs['receipts']).open():
        for r in json.loads(line).get('receipts', []):
            baseline[(r['kind'], r['key']['namespace'], r['key']['key'])] = str(r['id'])
    checks, seen, last_generation = [], set(), None
    for index, row in enumerate(measured):
        if row.get('sample') != index: raise ValueError('missing/reordered import sample')
        changes = json.loads((Path(inputs['imports']) / f'{index}.batch.json').read_text())
        receipts = row.get('receipts', row.get('receipt', {}).get('receipts'))
        if not isinstance(receipts, list) or len(receipts) != 137: raise ValueError('partial complete import receipts')
        ids = dict(baseline)
        generations = set()
        for item, (change, receipt) in enumerate(zip(changes, receipts)):
            image = change['image']; key = image['key']; ident = str(receipt['id'])
            if receipt['kind'] != image['kind'] or receipt['revision'] != 1 or ('item' in receipt and receipt['item'] != item):
                raise ValueError('wrong import receipt kind/revision/order')
            token = (receipt['kind'], ident)
            if token in seen: raise ValueError('reused import identity')
            seen.add(token); generations.add(receipt['generation'])
            ids[(image['kind'], key['namespace'], key['key'])] = ident
        if len(generations) != 1: raise ValueError('non-atomic import generations')
        generation = generations.pop()
        if last_generation is not None and generation <= last_generation: raise ValueError('non-monotone import generation')
        last_generation = generation
        expected, entities = [], []
        for change, receipt in zip(changes, receipts):
            image = change['image']; kind = image['kind']; ident = str(receipt['id'])
            properties = {k: {'list': [{'string': x} for x in v['string_list']]} if 'string_list' in v else v for k, v in image['properties'].items()}
            value = dict(id=ident, revision=1, properties=properties)
            if not metadata: value["key"] = image["key"]
            if kind == 'node':
                value.update(labels=sorted(image['labels']), text=None if metadata else image['text'],
                             vector_bits=None if metadata else image.get('vector_bits'))
            else:
                value.update(type=image['type'], **{name: ids[('node', image[name]['namespace'], image[name]['key'])] for name in ('source', 'target')})
            entities.append((kind, ident)); expected.append(value)
        sample_dir = directory / f'import-{index}'; sample_dir.mkdir()
        observed = inspect_entities(verifier, store, entities, sample_dir)
        if metadata:
            for value in observed:
                if value is not None: value.pop("key", None)
        checks.append(oracle_check(expected, observed, generation))
    return checks


def normalize_retention(measured, fixture, inputs, verifier, store, directory):
    checks, previous = [], 0
    for index, row in enumerate(measured):
        end = row['history_keys']
        created, deleted = row['created']['receipts'], row['deleted']['receipts']
        if end <= previous or len(created) != end - previous or len(deleted) != len(created): raise ValueError('partial retention history')
        for c, d in zip(created, deleted):
            if c['kind'] != 'node' or c['revision'] != 1 or d['revision'] != 2 or c['id'] != d['id'] or c['key'] != d['key']:
                raise ValueError('wrong retention creation/deletion receipt')
        sample_dir = directory / f'retention-{index}'; sample_dir.mkdir()
        observed = inspect_entities(verifier, store, [('node', r['id']) for r in deleted], sample_dir)
        checks.append(oracle_check([None] * len(deleted), observed, row['generation']))
        previous = end
    frozen = directory / 'baseline-entities.jsonl'
    with frozen.open('w') as output:
        run([verifier, 'expected-entities', fixture['path'], 'B' if inputs == fixture.get('B') else 'A', inputs['receipts']], stdout=output)
    with (directory / 'baseline-observations.jsonl').open('w') as output:
        run([verifier, 'inspect-entities', store, frozen], stdout=output)
    inventory = json.loads((Path(fixture['path']) / 'manifest.json').read_text())['inventory']
    # Cardinality is exact, independent of the worker's requested history count.
    return checks, previous, inventory['nodes'] + inventory['edges']


def normalize_repetition(manifest, cell, repetition, directory, command):
    raw = [json.loads(line) for line in (directory / 'samples.jsonl').read_text().splitlines()]
    measured = [r for r in raw if not r.get('warmup', False)]
    for row in measured:
        if row.get('status') != 0: raise ValueError('failed request is never a pass')
        if 'work_raw' in row:
            row['counters'] = public_counters(row)
        require_counters(row, query=not (cell.startswith(('structured-meeting-import', 'retention-')) or cell.startswith('mixed-4r1w/') and row.get('participant') == 4),
                         public_worker=cell.split('/')[2:3] in (['c'], ['swift']))
        if cell.startswith('mixed-4r1w/'): row['counters']['participant'] = row['participant']
        if 'ledger_before' in row or 'ledger_after' in row:
            before, after = row.get('ledger_before'), row.get('ledger_after')
            if not isinstance(before, dict) or not isinstance(after, dict): raise ValueError('missing write-ledger boundary')
            delta = {}
            for key in LEDGER_NAMES:
                if type(before.get(key)) is not int or type(after.get(key)) is not int or before[key] < 0 or after[key] < before[key]:
                    raise ValueError(f'missing/invalid ZE-76 write ledger {key}')
                delta[key] = after[key] - before[key]
            row['counters'].update(delta)
        elif cell.startswith('structured-meeting-import/') and cell.endswith('/rust') or cell.startswith('retention-'):
            raise ValueError('missing Rust write-ledger boundaries')

    monitor = [json.loads(line) for line in (directory / 'monitor.jsonl').read_text().splitlines()]
    process = json.loads((directory / 'process.json').read_text())
    protocol = manifest.get('cell_protocols', {}).get(cell, manifest)
    record = dict(cell=cell, repetition=repetition, process_nonce=process['process_nonce'],
                  warmups=sum(1 for r in raw if r.get('warmup', False)), samples_ns=[r['elapsed_ns'] for r in measured],
                  disposal_ns=[r.get('disposal_ns') for r in measured],
                  counters=[r.get('counters', {}) for r in measured],
                  duration_ms=process['duration_ms'], monitor=monitor, correct=False,
                  tainted=False, error=None, oracle_checks=[],
                  digests_before=process['digests_before'], digests_after=process['digests_after'])
    parts = cell.split('/')
    fixture = next(f for f in manifest['fixtures'] if f['scale'] == ('stress-10x' if parts[0] == 'stress-10x' else 'baseline'))
    state = parts[1] if len(parts) > 1 and parts[1] in ('A','B') else 'A'
    inputs = fixture[state]; jobs = Path(inputs['truth']).with_suffix('.jobs')
    if len(parts) == 5:
        name = parts[-1]
        if parts[0].startswith('paths-'): name += '-h' + parts[0].split('-')[1]
        for row in measured:
            row['name'] = name
            if 'case' not in row: row['case'] = row['schedule_index']
            job = json.loads((jobs / (str(row['case']) + '-' + name + '.json')).read_text())
            row['cohort'] = job['lexical_cohort'] if name == 'lexical-evidence' else job['cohort']
            row['normal_timing'] = job['normal_timing']
        annotated = directory / 'annotated-samples.jsonl'
        annotated.write_text(''.join(json.dumps(r) + '\n' for r in measured))
        verifier = Path(command[0]).parent / 'graph-workload'
        verified = subprocess.run([str(x) for x in (verifier, 'verify', fixture['path'], state, inputs['receipts'], jobs, annotated)], capture_output=True, text=True, cwd=ROOT)
        (directory / 'oracle.jsonl').write_text(verified.stdout)
        (directory / 'oracle.stderr').write_text(verified.stderr)
        record['oracle_checks'] = [json.loads(line) for line in verified.stdout.splitlines()]
        for check, row in zip(record['oracle_checks'], measured):
            check['cohort'] = row['cohort']; check['normal_timing'] = row['normal_timing']
        record['correct'] = verified.returncode == 0 and len(record['oracle_checks']) == len(measured) and all(c['correct'] for c in record['oracle_checks'])
        if verified.returncode: record['error'] = verified.stderr
    elif parts[0] == 'mixed-4r1w':
        verifier = Path(command[0]).parent / 'graph-workload'
        writes = sorted((r for r in measured if r['participant'] == 4), key=lambda r: r['sample'])
        write_checks = normalize_imports(writes, fixture, inputs, verifier, directory.parent / 'store', directory)
        for row in measured:
            if row['participant'] == 4:
                changes = json.loads((Path(inputs['imports']) / f"{row['sample']}.batch.json").read_text())
                receipts = row.get('receipts', row.get('receipt', {}).get('receipts'))
                row['input'] = {'changes': changes}
                row['receipt'] = {'receipts': [dict(r, key=c['image']['key']) for r, c in zip(receipts, changes)]}
            else:
                row['case'] = row.get('case', row.get('schedule_index'))
                row['name'] = 'alice-project-ranking'
        annotated = directory / 'annotated-samples.jsonl'
        annotated.write_text(''.join(json.dumps(r) + '\n' for r in measured))
        verified = subprocess.run([str(x) for x in (verifier, 'verify-mixed', fixture['path'], state, inputs['receipts'], jobs, annotated)], capture_output=True, text=True, cwd=ROOT)
        (directory / 'oracle.jsonl').write_text(verified.stdout)
        (directory / 'oracle.stderr').write_text(verified.stderr)
        checks = {(c['participant'], c['sample']): c for c in [json.loads(line) for line in verified.stdout.splitlines()]}
        checks.update({(4, i): c for i, c in enumerate(write_checks)})
        record['oracle_checks'] = [checks[(r['participant'], r['sample'])] for r in measured if (r['participant'], r['sample']) in checks]
        record['correct'] = verified.returncode == 0 and len(record['oracle_checks']) == len(measured)
        if verified.returncode: record['error'] = verified.stderr
    elif parts[0] in ('structured-meeting-import', 'cypher-meeting-metadata'):
        record['oracle_checks'] = normalize_imports(measured, fixture, inputs, Path(command[0]).parent / 'graph-workload',
            directory.parent / 'store', directory, metadata=parts[0] == 'cypher-meeting-metadata')
        record['correct'] = len(record['oracle_checks']) == protocol['samples']
    elif parts[0].startswith('recovery-'):
        tails = manifest['observed_tail_stores'][parts[0]]
        if len(measured) != len(tails): raise ValueError('partial recovery observations')
        for row, tail in zip(measured, tails):
            if 'truth_rows' not in tail or 'truth_generation' not in tail: raise ValueError('missing independent tail admission truth')
            compare_rows(tail['truth_rows'], row['rows'])
            if row['generation'] != tail['truth_generation']: raise ValueError('wrong recovery generation')
            record['oracle_checks'].append(dict(expected_rows=tail['truth_rows'], observed_rows=row['rows'],
                admitted_generation=row['generation'], truth_generation=tail['truth_generation'], truth_ids=[], hit_ids=[], correct=True))
        record['correct'] = True
    elif parts[0].startswith('retention-'):
        record['oracle_checks'], history, baseline_keys = normalize_retention(measured, fixture, inputs,
            Path(command[0]).parent / 'graph-workload', directory.parent / 'store', directory)
        record['correct'] = history == baseline_keys * int(parts[0].split('-')[1].rstrip('x'))
    if not record['correct'] and record['error'] is None:
        record['error'] = 'wrong or unverified complete results; no acceptance'
    record['tainted'] = environment_taint(record)
    (directory / 'repetition.json').write_text(json.dumps(record, indent=2))
    if record['error']:
        raise ValueError(record['error'] + '; raw samples/oracle/monitor retained, no qualification')
    return record


def environment_taint(record):
    if record['digests_before'] != record['digests_after']: return True
    previous, state, busy = 0, None, False
    observations = record['monitor']
    if not observations: return True
    for i, sample in enumerate(observations):
        t = sample.get('elapsed_ms')
        if not isinstance(t, int) or t < previous or t - previous > 1500 or (i == 0 and t > 1000): return True
        previous = t
        current = (sample.get('power'), sample.get('ac'), sample.get('qos'))
        cpu = sample.get('background_cpu_fraction')
        if sample.get('thermal') != 'nominal' or current[1] is not True or current[0] is None or current[2] is None: return True
        if state is not None and state != current: return True
        state = current
        if not isinstance(cpu, (int,float)) or not math.isfinite(cpu) or cpu < 0: return True
        if busy and cpu > 0.1: return True
        busy = cpu > 0.1
    return previous < record['duration_ms'] - 1000


def compare_rows(expected, actual, absolute=1e-6, relative=1e-6):
    if isinstance(expected, list):
        if not isinstance(actual, list) or len(expected) != len(actual):
            raise ValueError('partial/extra complete rows or list cells')
        for e, a in zip(expected, actual): compare_rows(e, a, absolute, relative)
    elif isinstance(expected, dict):
        if not isinstance(actual, dict) or set(expected) != set(actual) or len(expected) != 1:
            raise ValueError('wrong result cell type')
        tag = next(iter(expected))
        if tag == 'f64_bits':
            e = struct.unpack('<d', struct.pack('<Q', int(expected[tag], 16)))[0]
            a = struct.unpack('<d', struct.pack('<Q', int(actual[tag], 16)))[0]
            if not math.isfinite(e) or not math.isfinite(a) or abs(e-a) > absolute + relative * max(abs(e), abs(a)):
                raise ValueError('wrong independent full-domain score')
        elif tag == 'list': compare_rows(expected[tag], actual[tag], absolute, relative)
        elif expected[tag] != actual[tag]: raise ValueError('wrong complete result value/full ID')
    else:
        raise ValueError('invalid primitive result')


def copy_store(source, destination):
    source, destination = Path(source), Path(destination)
    size = sum(p.stat().st_size for p in source.rglob('*') if p.is_file())
    # One writable attempt, plus bounded WAL/artifact growth and a 1 GiB reserve.
    required = 2 * size + (1 << 30)
    parent = destination.parent
    while not parent.exists(): parent = parent.parent
    free = shutil.disk_usage(parent).free
    if free < required: raise ValueError(f'insufficient free disk: need {required}, have {free}')
    destination.mkdir(parents=True, exist_ok=False)
    records = []
    for path in sorted(source.rglob('*')):
        relative = path.relative_to(source); target = destination / relative
        if path.is_symlink(): raise ValueError('store copy refuses symlinks')
        if path.is_dir(): target.mkdir(exist_ok=True)
        elif path.is_file(): records.append(copy_verified(path, target, digest(path)))
        else: raise ValueError('unsupported store entry')
    return records


def write_report(args):
    source = Path(getattr(args, 'input', args.output if hasattr(args, 'output') else ''))
    records = source if source.is_file() else source / 'repetitions.jsonl'
    result = subprocess.run([str(Path(args.workers).resolve() / 'graph-workload'), 'report-smoke' if getattr(args, 'smoke', False) else 'report', args.manifest, str(records)], cwd=ROOT, capture_output=True, text=True)
    out = source.parent if source.is_file() else source
    if result.returncode:
        (out / 'RESULTS.md').write_text('ZE-77 evidence FAILED/BLOCKED\n\n' + result.stderr + '\nAll raw samples retained. No final qualification.\n')
        raise ValueError(result.stderr.strip())
    (out / 'summary.json').write_text(result.stdout)
    (out / 'RESULTS.md').write_text(('ZE-77 SMOKE pipeline evidence (no acceptance)\n\n' if getattr(args, 'smoke', False) else 'ZE-77 complete driver evidence\n\n') + result.stdout +
       '\nThreshold failures remain evidence. This report alone does not certify release/platform acceptance.\n')


def self_test_copy():
    with tempfile.TemporaryDirectory() as directory:
        root = Path(directory)
        src = root / 'source'
        src.write_bytes(b'approved fixture\0' * 100000)
        expected = digest(src)
        copy_verified(src, root / 'good', expected)
        src.write_bytes(b'tainted')
        try:
            copy_verified(src, root / 'bad', expected)
        except ValueError:
            if (root / 'bad').exists():
                raise AssertionError('failed copy exposed')
        else:
            raise AssertionError('digest drift accepted')
        # Existing destination must survive exclusive-create rejection.
        try:
            copy_verified(src, root / 'good', digest(src))
        except FileExistsError:
            assert digest(root / 'good') == expected
        else:
            raise AssertionError('immutable destination overwritten')
    print('ZE-77 streamed digest drift/exclusive destination PASS')


def main():
    p = argparse.ArgumentParser(description=__doc__)
    sub = p.add_subparsers(dest='command', required=True)
    b = sub.add_parser('build'); b.add_argument('--output', required=True); b.add_argument('--release', action='store_true'); b.add_argument('--smoke', action='store_true')
    sub.add_parser('self-test-copy')
    smoke = sub.add_parser('smoke'); smoke.add_argument('--workers', required=True); smoke.add_argument('--scale', choices=['small'], default='small'); smoke.add_argument('--output', required=True)
    prep = sub.add_parser('prepare'); prep.add_argument('--fixtures', nargs='+', default=[]); prep.add_argument('--smoke', action='store_true'); prep.add_argument('--manifest', required=True); prep.add_argument('--workers', default='target/ze77')
    measure = sub.add_parser('measure'); measure.add_argument('--workers', required=True); measure.add_argument('--manifest', required=True); measure.add_argument('--output', required=True); measure.add_argument('--observer', nargs='+'); measure.add_argument('--timeout', type=int, default=7200); measure.add_argument('--max-attempts', type=int, default=5); measure.add_argument('--smoke', action='store_true')
    report = sub.add_parser('report'); report.add_argument('--workers', default='target/ze77'); report.add_argument('--manifest', required=True); report.add_argument('--input', required=True); report.add_argument('--smoke', action='store_true')
    args = p.parse_args()
    if args.command == 'build': build_workers(args.output, args.release)
    elif args.command == 'self-test-copy': self_test_copy()
    elif args.command == 'prepare': prepare(args.fixtures, args.manifest, Path(args.workers).resolve(), args.smoke)
    elif args.command == 'measure': run_matrix(args)
    elif args.command == 'report': write_report(args)
    elif args.command == 'smoke':
        output = Path(args.output).resolve(); output.mkdir(parents=True, exist_ok=False)
        data = output / 'data'
        completed = subprocess.run([str(Path(args.workers).resolve() / 'graph-workload'), 'self-test'],
            cwd=ROOT, capture_output=True, text=True, env=dict(os.environ, ZE77_SMOKE_OUTPUT=str(data)))
        (output / 'smoke.log').write_text(completed.stdout + completed.stderr)
        if completed.returncode: raise ValueError('public small fixture smoke FAILED; raw evidence retained')
        failures = []
        swift_failed = False
        for name in READS:
            expected = json.loads((data / 'jobs' / f'{name}.json').read_text())['rows']
            for language, frontends in (('c', ('structured', 'cypher')), ('swift', ('structured', 'cypher'))):
                if language == 'swift' and swift_failed:
                    failures.append(f'swift/{name}: blocked after public worker crash; not run')
                    continue
                for frontend in frontends:
                    command = [Path(args.workers).resolve() / f'graph-workload-{language}', data / 'store', frontend, data / 'jobs' / f'{name}.{frontend}', '0', '1']
                    result = subprocess.run([str(x) for x in command], capture_output=True, text=True, cwd=ROOT)
                    (output / f'{language}-{frontend}-{name}.jsonl').write_text(result.stdout)
                    (output / f'{language}-{frontend}-{name}.stderr').write_text(result.stderr)
                    if result.returncode:
                        failures.append(f'{language}/{frontend}/{name}: exit {result.returncode}; raw evidence retained')
                        if language == 'swift': swift_failed = True
                        continue
                    try:
                        row = json.loads(result.stdout)
                        compare_rows(expected, row['rows'])
                    except (ValueError, KeyError) as error:
                        failures.append(f'{language}/{frontend}/{name}: {error}')
        imports = output / 'imports'; import_jobs(data / 'fixture', data / 'receipts.jsonl', imports, count=1)
        for language, suffix in (('c', 'batch'), ('swift', 'batch.json')):
            store = output / ('import-store-' + language); copy_store(data / 'store', store)
            result = subprocess.run([str(Path(args.workers).resolve() / f'graph-workload-{language}'), str(store), 'batch', str(imports / ('0.' + suffix)), '0', '1'], capture_output=True, text=True)
            (output / f'{language}-import.jsonl').write_text(result.stdout)
            (output / f'{language}-import.stderr').write_text(result.stderr)
            if result.returncode:
                failures.append(f'{language} supplied-payload import exit {result.returncode}; evidence retained')
                continue
            row = json.loads(result.stdout)
            if len(row['receipts']) != 137: failures.append(f'{language}: partial import receipts')
        (output / 'RESULTS.md').write_text(('FAILED focused smoke: ' + '; '.join(failures) + '\n\n' if failures else 'PASS focused smoke\n\n') + 'Executed small correctness checks: Rust/C structured and Cypher; typed Swift Cypher; C/Swift supplied-payload import.\n\nSwift Cypher SIGBUS remains blocked on ZE-313. Full host, retention, recovery and owner-selected model/cache-control inputs remain unqualified. Debug smoke timings are not baseline measurements.\n')
        if failures: raise ValueError('focused native smoke failed; see RESULTS.md and retained raw evidence')



if __name__ == '__main__':
    try:
        main()
    except (ValueError, OSError, subprocess.SubprocessError) as error:
        print(f'ZE-77: {error}', file=sys.stderr)
        sys.exit(1)
