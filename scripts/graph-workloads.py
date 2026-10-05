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
MISSING = {
    'ZE-76': 'preparation-inclusive deterministic work, shared interval actual capacity peaks, canonical/fence retention and sync/checkpoint observations',
    'ZE-278': 'typed Swift structured query construction/encoding and ZeppelinGraphStore.query',
    'ZE-71': 'matching graph archive/header/Swift identities and installed consumer handoff',
    'ZE-290': 'uninterrupted baseline ingestion, checkpoint/reopen and payload/identity proof',
    'ZE-287': 'legacy Intel section/archive measurement and owner size decision brief',
}


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
    # Current Swift has no structured query API. This builds the actual typed
    # wrapper, and its structured mode fails with ZE-278 rather than raw C.
    env.update(ZE_USE_LOCAL_FFI='1', ZE_LOCAL_FFI_ARCHIVE=str(archive), SWIFT_MODULECACHE_PATH=str(output / 'swift-cache'), CLANG_MODULE_CACHE_PATH=str(output / 'swift-cache'))
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


def prepare(fixtures, manifest_path, workers):
    inventory = []
    base = Path(manifest_path).resolve().parent
    base.mkdir(parents=True, exist_ok=True)
    failures = []
    for index, source in enumerate(fixtures):
        source = Path(source).resolve()
        raw = json.loads((source / 'manifest.json').read_text())
        destination = base / f'fixture-{index}'
        copies = copy_fixture(source, destination)
        run([workers / 'graph-fixture', 'validate', destination])
        inventory.append({'path': str(destination), 'scale': raw['scale'],
                          'manifest_sha256': digest(destination / 'manifest.json'), 'copies': copies})
        for state in ('A', 'B'):
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
            run([workers / 'graph-workload', 'truth', destination, state, receipts, truth])
            imports = base / f'imports-{index}-{state}'
            import_jobs(destination, receipts, imports)
            inventory[-1][state] = {'imports': str(imports), 'store': str(store), 'receipts': str(receipts),
                                    'truth': str(truth), 'truth_sha256': digest(truth)}
    manifest = accepted_manifest(inventory)
    manifest['preparation_failures'] = failures
    manifest['cell_protocols'] = {cell: {'warmups': 0 if cell.startswith(('structured-meeting-import','cypher-meeting-metadata','mixed-4r1w','recovery-')) else 50,
                                       'samples': 4200 if cell.startswith('mixed-4r1w') else 200 if 'meeting' in cell else 20 if cell.startswith('recovery-') else 1000,
                                       'target_p95_ns': 250_000_000 if cell.startswith(('baseline/','structured-meeting-import/','cypher-meeting-metadata/')) else 2_000_000_000 if cell.startswith('recovery-') else None} for cell in manifest['cells']}
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
        raise ValueError(f'failed cell retained at {output}; exit={status}')


def run_matrix(args):
    manifest = json.loads(Path(args.manifest).read_text())
    # Deliberately no date/time shortcut for either missing inputs or quiet host.
    missing = manifest.get('blocked_inputs', {})
    if missing:
        raise ValueError('authoritative measurement blocked: ' + '; '.join(f'{k}: {v}' for k, v in missing.items()))
    if manifest.get('preparation_failures'):
        raise ValueError('failed fixture preparation; cannot measure partial store')
    if not args.observer:
        raise ValueError('quiet M3 Max host requires actual 1-second thermal/power/AC/QoS/background-CPU observer')
    build = json.loads((Path(args.workers) / 'build.json').read_text())
    if build['profile'] != 'release':
        raise ValueError('authoritative cells require release opt-level=3 workers')
    for cell in manifest['cells']:
        for repetition in range(5):
            directory = Path(args.output) / cell / str(repetition)
            directory.mkdir(parents=True, exist_ok=False)
            accepted = None
            for attempt in range(args.max_attempts):
                attempt_dir = directory / ('attempt-' + str(attempt))
                attempt_dir.mkdir(parents=True, exist_ok=False)
                command = cell_command(manifest, cell, Path(args.workers).resolve(), attempt_dir)
                provenance = [Path(args.manifest), Path(args.workers) / 'build.json']
                for f in manifest['fixtures']:
                    provenance.extend(p for p in Path(f['path']).iterdir() if p.is_file())
                    for st in ('A','B'):
                        if st in f:
                            provenance.extend(p for p in Path(f[st]['truth']).with_suffix('.jobs').iterdir() if p.is_file())
                monitor_repetition(command, attempt_dir / 'worker', args.observer, args.timeout, provenance)
                record = normalize_repetition(manifest, cell, repetition, attempt_dir / 'worker', command)
                with (Path(args.output) / 'repetitions.jsonl').open('a') as output:
                    output.write(json.dumps(record) + '\n')
                if not record['tainted']:
                    accepted = record; break
            if accepted is None: raise ValueError('whole repetitions remain tainted; all attempts retained, qualification blocked')




def cell_command(manifest, cell, workers, directory):
    parts = cell.split('/')
    scale = 'stress-10x' if cell.startswith('stress-10x') else 'baseline'
    fixture = next((f for f in manifest['fixtures'] if f['scale'] == scale), None)
    if fixture is None: raise ValueError(f'missing digest-checked {scale} fixture')
    state = parts[1] if len(parts) > 1 and parts[1] in ('A', 'B') else 'A'
    inputs = fixture.get(state)
    if inputs is None: raise ValueError(f'missing complete {scale}/{state} preparation; ZE-290 uninterrupted ingestion handoff required')
    store = directory / 'store'
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
            suffix = 'structured' if language == 'c' else 'cypher'
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
            if not expected or not capture.get('checkpoint_boundary_observed') or 'creation_serial_count' not in capture or 'full_syncs' not in capture or 'directory_syncs' not in capture:
                raise ValueError('missing ZE-76 observed tail-boundary/creation-serial/sync capture; requested envelope count is not proof')
            target = directory / ('tail-' + str(index))
            copies = copy_store(tail['path'], target)
            (directory / ('tail-' + str(index) + '-copy.json')).write_text(json.dumps(copies, indent=2))
            paths.append(str(target.resolve()))
        listing = directory / 'tails.list'; listing.write_text('\n'.join(paths) + '\n')
        return [workers / 'graph-workload', 'recovery-series', listing, 'read-write']
    if parts[0] == 'model-resident':
        if not manifest.get('model'): raise ValueError('missing accepted model/runtime/precision and actual model memory observer')
        raise ValueError('missing ZE-76 model/process versus managed-memory observation handoff')
    if parts[0] == 'os-cold':
        if not manifest.get('cache_control'): raise ValueError('missing validated cache-control method; fresh process does not mean OS-cold')
        raise ValueError('cache-control validation evidence required before OS-cold execution')
    raise ValueError(f'undeclared execution protocol {cell}')


def normalize_repetition(manifest, cell, repetition, directory, command):
    raw = [json.loads(line) for line in (directory / 'samples.jsonl').read_text().splitlines()]
    measured = [r for r in raw if not r.get('warmup', False)]
    # Current ABI/Swift expose real global work rows, distinct from per-call
    # ranges. Map only known counters; absent ZE-76 fields stay absent.
    names = {0:'operator_rows',1:'adjacency_entries',5:'result_bytes',
             6:'prepared_payload_bytes',7:'completed_abi_bytes',8:'vector_coordinates',
             9:'vector_payload_bytes',10:'postings',11:'lexical_blocks',12:'search_calls',
             13:'directory_lookups',14:'scans',15:'paths',16:'rows_in',17:'rows_out',
             18:'join_probes',19:'group_keys',20:'eligibility_entries',21:'copied_bytes'}
    for row in measured:
        if 'counters' not in row and 'work_raw' in row:
            work = row['work_raw']
            if 'global_work' in row:
                span = row['global_work']; start, count = span['start'], span['count']
                if start < 0 or count < 0 or start + count > len(work): raise ValueError('invalid global work range')
                work = work[start:start+count]
            counters = {}
            for entry in work:
                if entry['kind'] in names:
                    name = names[entry['kind']]
                    if name in counters: raise ValueError('duplicate global work counter')
                    counters[name] = entry['value']
            row['counters'] = counters

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
        record['correct'] = verified.returncode == 0 and all(c['correct'] for c in record['oracle_checks'])
        if verified.returncode: record['error'] = verified.stderr
    elif parts[0] == 'mixed-4r1w':
        verified = subprocess.run([str(x) for x in (Path(command[0]).parent / 'graph-workload', 'verify-mixed', fixture['path'], state, inputs['receipts'], jobs, directory / 'samples.jsonl')], capture_output=True, text=True, cwd=ROOT)
        (directory / 'oracle.jsonl').write_text(verified.stdout)
        (directory / 'oracle.stderr').write_text(verified.stderr)
        record['oracle_checks'] = [json.loads(line) for line in verified.stdout.splitlines()]
        record['correct'] = verified.returncode == 0
        if verified.returncode: record['error'] = verified.stderr
    required = ('managed_peak_bytes','directory_lookups','pages_decoded','adjacency_entries',
                'operator_rows','canonical_bytes','vector_coordinates','vector_payload_bytes',
                'postings','eligible_cardinality','candidate_window_peak','search_calls',
                'result_bytes','full_syncs','directory_syncs','checkpoint_ns')
    if any(any(key not in row for key in required) for row in record['counters']):
        record['error'] = 'ZE-76 full actual-capacity/resource/work observations missing'
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
    source = Path(args.input)
    records = source if source.is_file() else source / 'repetitions.jsonl'
    result = subprocess.run([str(Path(args.workers).resolve() / 'graph-workload'), 'report', args.manifest, str(records)], cwd=ROOT, capture_output=True, text=True)
    out = source.parent if source.is_file() else source
    if result.returncode:
        (out / 'RESULTS.md').write_text('ZE-77 evidence FAILED/BLOCKED\n\n' + result.stderr + '\nAll raw samples retained. No final qualification.\n')
        raise ValueError(result.stderr.strip())
    (out / 'summary.json').write_text(result.stdout)
    (out / 'RESULTS.md').write_text('ZE-77 complete driver evidence\n\n' + result.stdout +
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
    b = sub.add_parser('build'); b.add_argument('--output', required=True); b.add_argument('--release', action='store_true')
    sub.add_parser('self-test-copy')
    smoke = sub.add_parser('smoke'); smoke.add_argument('--workers', required=True); smoke.add_argument('--scale', choices=['small'], default='small'); smoke.add_argument('--output', required=True)
    prep = sub.add_parser('prepare'); prep.add_argument('--fixtures', nargs='+', required=True); prep.add_argument('--manifest', required=True); prep.add_argument('--workers', default='target/ze77')
    measure = sub.add_parser('measure'); measure.add_argument('--workers', required=True); measure.add_argument('--manifest', required=True); measure.add_argument('--output', required=True); measure.add_argument('--observer', nargs='+'); measure.add_argument('--timeout', type=int, default=7200); measure.add_argument('--max-attempts', type=int, default=5)
    report = sub.add_parser('report'); report.add_argument('--workers', default='target/ze77'); report.add_argument('--manifest', required=True); report.add_argument('--input', required=True)
    args = p.parse_args()
    if args.command == 'build': build_workers(args.output, args.release)
    elif args.command == 'self-test-copy': self_test_copy()
    elif args.command == 'prepare': prepare(args.fixtures, args.manifest, Path(args.workers).resolve())
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
            for language, frontends in (('c', ('structured', 'cypher')), ('swift', ('cypher',))):
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
        rejected = subprocess.run([str(Path(args.workers).resolve() / 'graph-workload-swift'), str(data / 'store'), 'structured', 'unused', '0', '1'], capture_output=True, text=True)
        (output / 'swift-structured-blocked.log').write_text(rejected.stderr)
        if rejected.returncode == 0 or 'ZE-278' not in rejected.stderr:
            raise ValueError('Swift structured missing-input gate failed')
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
        (output / 'RESULTS.md').write_text(('FAILED focused smoke: ' + '; '.join(failures) + '\n\n' if failures else 'PASS focused smoke\n\n') + 'Executed small correctness checks: Rust/C structured and Cypher; typed Swift Cypher; C/Swift supplied-payload import.\n\nSwift structured blocked on ZE-278. Qualification consolidation remains a strict prepare gate; ZE-76/ZE-71/ZE-290/ZE-287 and quiet M3 Max inputs remain unqualified. Debug smoke timings are not baseline measurements.\n')
        if failures: raise ValueError('focused native smoke failed; see RESULTS.md and retained raw evidence')



if __name__ == '__main__':
    try:
        main()
    except (ValueError, OSError, subprocess.SubprocessError) as error:
        print(f'ZE-77: {error}', file=sys.stderr)
        sys.exit(1)
