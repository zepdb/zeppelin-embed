#!/usr/bin/env python3
"""ZE-74 shared expected-value and receipt coordinator; no release qualification by omission."""
import argparse
import copy
import hashlib
import json
import os
from pathlib import Path
import subprocess
import sys
import tempfile
import unittest

sys.path.insert(0, str(Path(__file__).resolve().parent))
from check_swift_qualification import validate_swift_tests

ROOT = Path(__file__).resolve().parents[1]
DEFAULT = ROOT / 'bindings/fixtures/graph_profile_parity_v1.json'


def digest(path):
    return hashlib.sha256(Path(path).read_bytes()).hexdigest()


def build_manifest(path=DEFAULT):
    m = json.loads(Path(path).read_text())
    if m['format'] != 'graph-profile-parity-v1':
        raise ValueError('unsupported manifest version')
    source = ROOT / m['source_profile']
    if digest(source) != m['source_profile_sha256']:
        raise ValueError('stale ZE-59 profile pin')
    source_rows = {r['coordinate']: r for r in json.loads(source.read_text())['scenarios']}
    ids = set()
    for row in m['scenarios'] + m['local']:
        if row['id'] in ids:
            raise ValueError('duplicate case ' + row['id'])
        ids.add(row['id'])
        if 'source' in row:
            original = source_rows[row['id']]
            if row['source']['sha256'] != original['fixture_block_sha256'] or row['source']['hashes'] != original['hashes']:
                raise ValueError('stale original coordinate ' + row['id'])
            block = next(b for b in (source.parent / original['fixture']).read_text().split('\n=== ')[1:]
                         if b.splitlines()[0] == row['id'])
            if block != row['fixture_block']:
                raise ValueError('modified original scenario ' + row['id'])
    if {r['id'] for r in m['scenarios']} != set(source_rows):
        raise ValueError('missing ZE-59 coordinates')
    m['manifest_sha256'] = digest(path)
    return m


def validate_receipts(manifest, receipts, required):
    seen = set()
    for row in receipts:
        key = (row['case'], row['path'])
        if row['state'] == 'N/A with reason':
            if not row.get('reason') or key in required:
                raise ValueError('invalid N/A receipt ' + str(key))
            continue
        if key in seen:
            raise ValueError('duplicate receipt ' + str(key))
        seen.add(key)
        if row.get('manifest_sha256') != manifest['manifest_sha256']:
            raise ValueError('stale receipt ' + str(key))
        if row['state'] != 'focused GREEN' or not row.get('reopened') or not row.get('released'):
            raise ValueError('partial execution receipt ' + str(key))
        declared = next((c for c in manifest.get('cases', []) if c['id'] == row['case']), None)
        expected_rows = declared['expected_rows'] if declared else row.get('expected_rows')
        if not row.get('error') and row.get('rows') != expected_rows:
            raise ValueError('independent expected-value mismatch ' + str(key))
        if declared and row.get('effects') != declared['effects']:
            raise ValueError('independent side-effect mismatch ' + str(key))
    if seen != set(required):
        raise ValueError('missing or extra receipts: ' + str(set(required) ^ seen))
    return receipts


def compare_paths(receipts):
    by_case = {}
    for r in receipts:
        by_case.setdefault(r['case'], []).append(r)
    for case, rows in by_case.items():
        observed = [(r.get('rows'), r.get('effects')) for r in rows if r['state'] == 'focused GREEN']
        if observed and any(r != observed[0] for r in observed[1:]):
            raise ValueError('cross-path semantic mismatch ' + case)


def compare_c_consumer(cases, observations):
    seen = set()
    for row in observations:
        case = next(c for c in cases if c['id'] == row['case'])
        key=(row['case'],row.get('path','c-cypher'))
        if key in seen:
            raise ValueError('duplicate installed C case/path')
        seen.add(key)
        if row['after'] != row['reopened_snapshot']:
            raise ValueError('installed C durable state mismatch ' + row['case'])
        if case['error']:
            if row['status'] != case['error']['c_code'] or row['before'] != row['after']:
                raise ValueError('installed C rejection mismatch ' + row['case'])
            continue
        if row['status'] != 0:
            raise ValueError('installed C failed statement ' + row['case'])
        def normalize(cell):
            cell = copy.deepcopy(cell)
            if cell['type'] == 3:
                cell['value'] = float(cell['value'])
            elif cell['type'] == 7:
                cell['value'] = [normalize(c) for c in cell['value']]
            elif cell['type'] in (5, 6):
                image = cell['value']
                image['properties'] = {k: normalize(v) for k, v in image['properties'].items()}
                if cell['type'] == 5:
                    image['labels'].sort()
            return cell
        canonical = lambda rows: sorted(json.dumps([normalize(c) for c in r], sort_keys=True) for r in rows)
        if canonical(row['rows']) != canonical(case['expected_cells']):
            raise ValueError('installed C primitive mismatch ' + row['case'])
        expected_columns = list(case.get('column_mapping', {})) if row.get('path') == 'c-structured' else case['header']
        if case.get('column_kinds') != row.get('column_kinds'):
            raise ValueError('installed C column kind mismatch ' + row['case'])
        if expected_columns and row['columns'] != expected_columns:
            raise ValueError('installed C column mismatch ' + row['case'])
        def state(snapshot):
            nodes, rels = snapshot
            ids = [set(), set()]
            labels, props = set(), set()
            for domain, entities in enumerate((nodes, rels)):
                for identity, entity in entities:
                    ident = identity['value']
                    ids[domain].add(ident)
                    image = entity['value']
                    if domain == 0:
                        labels.update(image['labels'])
                    for name, value in image['properties'].items():
                        props.add((domain, ident, name, json.dumps(value, sort_keys=True)))
            return ids + [labels, props]
        before, after = state(row['before']), state(row['after'])
        effects = [n for old, new in zip(before, after) for n in (len(new-old), len(old-new))]
        if effects != case['effects']:
            raise ValueError('installed C eight side-effect mismatch ' + row['case'])
    required={(c['id'],'c-cypher') for c in cases}
    required.update((c['id'],'c-structured') for c in cases if c['structured'])
    if seen != required:
        raise ValueError('missing installed C cases/paths')


def percentile(samples, percent):
    import math
    if not samples or not 0 < percent <= 100 or any(not math.isfinite(v) or v < 0 for v in samples):
        raise ValueError('invalid latency samples')
    return sorted(samples)[math.ceil(len(samples) * percent / 100) - 1]


def repetition_state(samples, monitor, expected_requests, pins_before, pins_after):
    if len(samples) != expected_requests or any(row.get('error') or not row.get('correct') for row in samples):
        return 'failed'
    if pins_before != pins_after or not monitor:
        return 'tainted'
    for i, row in enumerate(monitor):
        if row.get('thermal') != 'nominal' or row.get('missing'):
            return 'tainted'
        if i and (row['time'] - monitor[i-1]['time'] > 1.5 or
                  any(row[k] != monitor[0][k] for k in ('power', 'ac', 'qos')) or
                  (row['nonbenchmark_cpu'] > .10 and monitor[i-1]['nonbenchmark_cpu'] > .10)):
            return 'tainted'
    if monitor[-1]['time'] < samples[-1]['completed_at'] or monitor[0]['time'] > samples[0]['started_at']:
        return 'tainted'
    return 'valid'


def run_matrix(args):
    m = build_manifest(args.manifest)
    args.output.mkdir(parents=True, exist_ok=True)
    env = dict(os.environ, CARGO_BUILD_JOBS='3')
    command = ['cargo', 'build', '-p', 'zeppelin-embed-workspace-tests', '--features', 'graph-cypher', '--bin', 'graph-profile']
    subprocess.run(command, cwd=ROOT, env=env, check=True)
    described = subprocess.check_output([ROOT / 'target/debug/graph-profile', 'describe'], cwd=ROOT, text=True)
    m['cases'] = json.loads(described)['cases']
    (args.output / 'consumer-manifest.json').write_text(described)
    receipts = []
    failures = []
    for case in m['cases']:
        for path in ['rust-cypher', 'c-cypher', 'rust-structured', 'c-structured']:
            if path.endswith('structured') and case['structured'] is None:
                compiler_only = case['error'] is not None and case['error']['stage'] == 'compile'
                receipts.append(dict(case=case['id'], path=path, state='N/A with reason' if compiler_only else 'planned',
                                     reason='compiler-only source refusal has no equivalent valid typed query' if compiler_only else 'ZE-74 independent original structured translation unfinished',
                                     manifest_sha256=m['manifest_sha256']))
                continue
            result = subprocess.run([ROOT / 'target/debug/graph-profile', 'run', case['id'], path],
                                    cwd=ROOT, capture_output=True, text=True)
            (args.output / (case['id'].replace('/', '-') + '-' + path + '.log')).write_text(result.stdout + result.stderr)
            if result.returncode:
                failures.append(case['id'] + ' ' + path + ': ' + result.stderr.strip())
                receipts.append(dict(case=case['id'], path=path, state='failed', reason=result.stderr.strip()))
            else:
                receipt = json.loads(result.stdout)
                receipt['manifest_sha256'] = m['manifest_sha256']
                receipts.append(receipt)
    swift_receipts = args.output / 'swift-receipts.jsonl'
    swift_receipts.unlink(missing_ok=True)
    swift_env = dict(env, ZE_USE_LOCAL_FFI='1', ZE74_SWIFT_MANIFEST=str((args.output/'consumer-manifest.json').resolve()),
                     ZE74_SWIFT_RECEIPTS=str(swift_receipts.resolve()), CLANG_MODULE_CACHE_PATH=str(ROOT/'target/swift-ze74-clang'),
                     SWIFTPM_MODULECACHE_OVERRIDE=str(ROOT/'target/swift-ze74-module'))
    archive = subprocess.run(['cargo', 'build', '--locked', '--release', '-p', 'zeppelin-embed-ffi', '--features', 'graph-cypher'],
                             cwd=ROOT, env=env, capture_output=True, text=True)
    (args.output/'swift-archive.log').write_text(archive.stdout+archive.stderr)
    if archive.returncode == 0:
        swift = subprocess.run(['swift', 'test', '--disable-sandbox', '--package-path', 'bindings/swift/graph',
                                '--scratch-path', 'target/swift-ze74', '--cache-path', 'target/swift-ze74-cache',
                                '--jobs', '3', '--filter', 'GraphProfileParityTests'], cwd=ROOT, env=swift_env,
                               capture_output=True, text=True)
        (args.output/'swift.log').write_text(swift.stdout+swift.stderr)
        if swift.returncode:
            diagnostics = [line for line in (swift.stdout + swift.stderr).splitlines() if 'error:' in line]
            failures.extend(diagnostics or ['Swift runner exit ' + str(swift.returncode) + '; see swift.log'])
        try:
            validate_swift_tests(swift.stdout + swift.stderr, 'GraphProfileParityTests')
        except ValueError as error:
            failures.append(str(error))
        if swift_receipts.exists():
            for line in swift_receipts.read_text().splitlines():
                r = json.loads(line)
                r['manifest_sha256'] = m['manifest_sha256']
                receipts.append(r)
    else:
        failures.append('Swift archive build failed; see swift-archive.log')
    (args.output / 'receipts.json').write_text(json.dumps(receipts, indent=2) + '\n')
    if failures:
        raise ValueError("; ".join(failures))
    compare_paths(receipts)
    report = dict(scope='source focused evidence only', receipts=receipts, failures=failures,
                  missing_inputs=m['missing_inputs'], original_corpus='Rust/C Cypher original corpus executed; original structured translations unfinished',
                  swift='source Cypher tests executed; ZE-278 public list parameter and structured paths unavailable',
                  integrated='BLOCKED: ZE-72 receipts and ZE-29 revision required')
    (args.output / 'report.json').write_text(json.dumps(report, indent=2) + '\n')
    required = {(c['id'], path) for c in m['cases'] for path in m['paths']
                if not (path.endswith('structured') and c['structured'] is None and c['error'] and c['error']['stage'] == 'compile')}
    try:
        validate_receipts(m, receipts, required)
    except ValueError as error:
        raise ValueError('incomplete matrix: ZE-72, ZE-278, ZE-71 and unfinished ZE-74 cells; ' + str(error)) from error


class Controls(unittest.TestCase):
    def test_missing_duplicate_stale_partial_receipts(self):
        manifest = {'manifest_sha256': 'pin'}
        good = dict(case='bag', path='rust', state='focused GREEN', manifest_sha256='pin',
                    rows=['[Int(7)]', '[Int(7)]'], expected_rows=['[Int(7)]', '[Int(7)]'],
                    effects=[0]*8, reopened=True, released=True)
        required = {('bag', 'rust')}
        for bad in ([], [good, good], [dict(good, manifest_sha256='stale')], [dict(good, reopened=False)]):
            with self.assertRaises(ValueError):
                validate_receipts(manifest, bad, required)
        validate_receipts(manifest, [good], required)

    def test_receipt_cannot_rewrite_independent_expectation(self):
        declared = dict(id='bag', expected_rows=['Int(7)', 'Int(7)'], effects=[0]*8)
        manifest = dict(manifest_sha256='pin', cases=[declared])
        forged = dict(case='bag', path='rust', state='focused GREEN', manifest_sha256='pin',
                      rows=['Int(7)'], expected_rows=['Int(7)'], effects=[0]*8, reopened=True, released=True)
        with self.assertRaises(ValueError):
            validate_receipts(manifest, [forged], {('bag', 'rust')})

    def test_cross_path_wrong_bag_or_type(self):
        good = dict(case='bag', path='rust', state='focused GREEN', rows=['Int(7)', 'Int(7)'], effects=[0]*8)
        for rows in (['Int(7)'], ['Float(7.0)', 'Int(7)']):
            with self.assertRaises(ValueError):
                compare_paths([good, dict(good, path='c', rows=rows)])

    def test_whole_repetition_taint_and_percentiles(self):
        self.assertEqual(percentile([1, 2, 3, 4], 95), 4)
        samples = [dict(correct=True, started_at=0, completed_at=2)]
        monitor = [dict(time=t, thermal='nominal', power='normal', ac=True, qos='default', nonbenchmark_cpu=0) for t in range(3)]
        self.assertEqual(repetition_state(samples, monitor, 1, 'pin', 'pin'), 'valid')
        bad = copy.deepcopy(monitor)
        bad[1]['nonbenchmark_cpu'] = bad[2]['nonbenchmark_cpu'] = .11
        self.assertEqual(repetition_state(samples, bad, 1, 'pin', 'pin'), 'tainted')
        self.assertEqual(repetition_state(samples, monitor, 2, 'pin', 'pin'), 'failed')
        self.assertEqual(repetition_state(samples, monitor, 1, 'pin', 'changed'), 'tainted')

    def test_swift_qualification_requires_execution_without_skips(self):
        good = "Test Suite 'GraphProfileParityTests' passed at now.\nExecuted 2 tests, with 0 failures (0 unexpected) in 1 second"
        validate_swift_tests(good, 'GraphProfileParityTests')
        for bad in ('', good.replace('2 tests', '0 tests'),
                    good.replace('with 0 failures', 'with 1 test skipped and 0 failures'),
                    good.replace('with 0 failures', 'with 2 tests skipped and 0 failures')):
            with self.assertRaises(ValueError):
                validate_swift_tests(bad, 'GraphProfileParityTests')

    def test_profile_pins(self):
        build_manifest()


def generate_c(manifest):
    text = '/* Generated from graph_profile_parity_v1.json. */\n'
    text += 'typedef struct {const char *id;const char *query;const char *const *setup;size_t setup_count;unsigned structured;} Ze74Case;\n'
    for i, case in enumerate(manifest['local']):
        text += f'static const char *const setup_{i}[] = {{' + ','.join(json.dumps(q) for q in case['setup'] or ['']) + '};\n'
    text += 'static const Ze74Case ze74_cases[] = {\n'
    for i, case in enumerate(manifest['local']):
        text += '{' + json.dumps(case['id']) + ',' + json.dumps(case['query']) + f',setup_{i},' + str(len(case['setup'])) + ',' + str(int(case['structured'] is not None)) + '},\n'
    text += '};\nstatic int32_t ze74_structured(size_t index, ZeGraphHandle h, ZeGraphResponse *out) { switch(index) {\n'
    kinds = dict(unit=0, scan=4, eager=5, mutate=6, aggregate=7, limit=9, expand=13, bounded=14, optional=15, project=16, with_=17)
    for index, case in enumerate(manifest['local']):
        if not case['structured']:
            continue
        p = copy.deepcopy(case['structured'])
        if len(p['operators']) > 1 and p['operators'][0]['kind'] == 'unit' and p['operators'][1]['kind'] == 'scan':
            p['operators'].pop(0); p['root'] -= 1
            for op in p['operators']:
                op['inputs'] = [] if op['kind'] == 'scan' else [v-1 for v in op['inputs']]
        data = bytearray()
        def span(s):
            start = len(data); data.extend(s.encode()); return '{'+str(start)+','+str(len(s.encode()))+'}'
        values, expressions, children = [], [], []
        for e in p['expressions']:
            fields = '.abi_size=sizeof(ZeGraphExpression)'
            if e['kind'] == 'literal':
                v = '.abi_size=sizeof(ZeGraphValue)'
                tag = dict(null=0, bool=1, integer=2, float=3, string=4)[e['type']]
                v += f',.tag={tag}'
                if tag == 1: v += ',.boolean='+str(int(e['value']))
                elif tag == 2: v += ',.integer='+str(e['value'])
                elif tag == 3: v += ',.floating='+repr(e['value'])
                elif tag == 4: v += ',.range='+span(e['value'])
                fields += ',.value='+str(len(values)); values.append('{'+v+'}')
            elif e['kind'] == 'slot': fields += ',.kind=1,.value='+str(e['value'])
            elif e['kind'] == 'list':
                fields += ',.kind=7,.children={'+str(len(children))+','+str(len(e['items']))+'}'
                children.extend(e['items'])
            else:
                fields += ',.kind=8,.operation='+str(int(e['kind'] == 'collect'))
                if e['operand'] is not None: fields += ',.has_operand=1,.left='+str(e['operand'])
            expressions.append('{'+fields+'}')
        operators, inputs, projections = [], [], []
        for o in p['operators']:
            fields = '.abi_size=sizeof(ZeGraphOperator),.kind='+str(kinds['with_' if o['kind']=='with' else o['kind']])
            if o['inputs']:
                fields += ',.inputs={'+str(len(inputs))+','+str(len(o['inputs']))+'}'
                inputs.extend(o['inputs'])
            for key in ('projections', 'aggregates'):
                selected = o.get(key, [])
                if selected:
                    fields += ',.'+key+'={'+str(len(projections))+','+str(len(selected))+'}'
                    projections.extend(p['projections'][i] for i in selected)
            if o['kind'] == 'scan' and o.get('label'): fields += ',.has_name=1,.name='+span(o['label'])
            if o['kind'] in ('expand', 'bounded'):
                fields += ',.node_slot=2,.relationship_slot=1'
                if o['kind']=='bounded': fields += f',.path_min={o["min"]},.path_max={o["max"]}'
            if o['kind']=='optional': fields += ',.predicate={1,'+str(o['predicate'])+'}'
            if o['kind']=='mutate': fields += ',.mutations={0,1}'
            if o['kind']=='limit': fields += ',.has_limit=1,.limit='+str(o['limit'])
            operators.append('{'+fields+'}')
        text += f'case {index}: {{\n'
        def array(ctype, name, entries):
            nonlocal text
            text += f'{ctype} {name}[] = {{' + ','.join(entries or ['{0}']) + '};\n'
        array('ZeGraphValue', 'values', values)
        array('ZeGraphExpression', 'expressions', expressions)
        array('ZeGraphOperator', 'operators', operators)
        array('ZeGraphProjection', 'projections', ['{.abi_size=sizeof(ZeGraphProjection),.slot='+str(v['slot'])+',.expression='+str(v['expression'])+'}' for v in projections])
        text += 'uint32_t inputs[] = {'+','.join(map(str, inputs or [0]))+'};\n'
        text += 'uint32_t children[] = {'+','.join(map(str, children or [0]))+'};\n'
        text += 'const uint8_t bytes[] = '+json.dumps(data.decode())+';\n'
        text += 'ZeGraphMutation mutation = {.abi_size=sizeof(ZeGraphMutation),.output=3};\n'
        text += 'ZeGraphValuePool pool = {.abi_size=sizeof(ZeGraphValuePool),.bytes=bytes,.byte_count='+str(len(data))+',.values=values,.value_count='+str(len(values))+'};\n'
        text += 'ZeGraphPlan plan = {.abi_size=sizeof(ZeGraphPlan),.root='+str(p['root'])+',.pool=&pool,.expressions=expressions,.expression_count='+str(len(expressions))+',.operators=operators,.operator_count='+str(len(operators))+',.inputs=inputs,.input_count='+str(len(inputs))+',.projections=projections,.projection_count='+str(len(projections))+',.expression_children=children,.expression_child_count='+str(len(children))+',.mutations=&mutation,.mutation_count=1};\n'
        text += 'ZeGraphQueryRequest request = {.abi_size=sizeof(ZeGraphQueryRequest),.plan=&plan}; return ze_graph_query(h,&request,out);}\n'
    return text + 'default: fprintf(stderr,"ZE-74: no declared structured plan\\n"); exit(2); }}\n'


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    sub = parser.add_subparsers(dest='command', required=True)
    sub.add_parser('self-test')
    for command in ('run', 'measure', 'generate-c'):
        p = sub.add_parser(command)
        p.add_argument('--manifest', type=Path, default=DEFAULT)
        p.add_argument('--output', type=Path, default=ROOT/'tasks/evidence/ze-74')
        if command == 'measure':
            p.add_argument('--fixture', type=Path, required=True)
            p.add_argument('--paths', default='rust,c,swift')
            p.add_argument('--repetitions', type=int, default=5)
            p.add_argument('--warmup', type=int, default=50)
            p.add_argument('--requests', type=int, default=1000)
            p.add_argument('--imports', type=int, default=200)
    args = parser.parse_args()
    if args.command == 'self-test':
        unittest.main(argv=[__file__])
    elif args.command == 'run':
        run_matrix(args)
    elif args.command == 'measure':
        m = build_manifest(args.manifest)
        args.output.mkdir(parents=True, exist_ok=True)
        report = dict(state='blocked', required_cells=m['timing_cells'], missing_inputs=['ZE-76', 'ZE-278', 'ZE-290'],
                      reason='ZE-76 qualified counters, ZE-278 Swift structured API and ZE-290 sustained ingestion are not on main; no timing qualification',
                      implementation_remaining='ZE-74 complete fixture request/import/mixed-load driver and retention/provenance reporting remain unfinished')
        (args.output/'timing-blocked.json').write_text(json.dumps(report, indent=2)+'\n')
        raise ValueError(report['reason'])
    else:
        m = build_manifest(args.manifest)
        args.output.mkdir(parents=True, exist_ok=True)
        text = generate_c(m)
        (args.output/'graph_profile_cases.h').write_text(text)


if __name__ == '__main__':
    try:
        main()
    except (ValueError, subprocess.CalledProcessError) as error:
        print('graph-profile-parity: '+str(error), file=sys.stderr)
        sys.exit(1)
