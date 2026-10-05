#!/usr/bin/env python3
"""ZE-78 retained-evidence gate. Does not run campaigns or publish artifacts.

A receipt is not a measurement producer. Every qualified cell binds a normalized
JSON report, raw log and artifacts to the current source/build digest. Producers
must retain their raw evidence. Owner exclusions require a separately supplied,
reviewed decisions file; a manifest cannot authorize its own exclusions.
"""
import argparse
import hashlib
import json
import math
import re
from pathlib import Path
import subprocess
import sys
import tomllib

ROOT = Path(__file__).resolve().parents[2]
CRATES = {**{f'zeppelin-embed{suffix}': f'crates/zeppelin-embed{suffix}'
             for suffix in ('', '-cypher', '-text', '-ffi', '-bench')},
          'zeppelin-embed-adversarial-oracle': 'tests/adversarial-oracle',
          'zeppelin-embed-workspace-tests': 'tests'}
EXCLUSIONS = ['registry/', 'fuzz/', 'target/',
              'crates/zeppelin-embed-bench/src/bin/',
              'crates/zeppelin-embed-bench/src/platform/',
              'crates/zeppelin-embed-bench/src/recall/']
SHAPES = ('project-evidence', 'semantic-context', 'alice-project-ranking',
          'lexical-evidence', 'hybrid-project-evidence', 'bounded-evidence')
SURFACES = ('rust', 'c', 'swift')
FAULTS = ('replay', 'staging', 'exclusive-create', 'artifact-sync', 'wal-prefix',
          'commit-view-swap', 'query-materialization', 'adjacency', 'checkpoint-reclaim',
          'disk-close-race', 'kill-9')
LIMITS = dict(operators=4096, expressions=4096, depth=64, parameters=256,
              columns=256, hops=16, list_depth=16, list_elements=524288,
              eligible_ids=524288, search_calls=8, search_k=4096, candidates=65536,
              adjacency=2000000, rows_visited=4000000, evaluations=8000000,
              hash_probes=16000000, result_rows=65536, result_bytes=4*1024**2,
              coordinates=2147483648, vector_bytes=8*1024**3, postings=64000000,
              blocks=4000000, terms=64, maintenance_bytes=32*1024**2,
              merge_bytes=4*1024**2, changes=16384, input_bytes=8*1024**2,
              cypher_bytes=65536, tokens=8192, ast_nodes=4096,
              query_bytes=24*1024**2, write_bytes=64*1024**2,
              cache_bytes=32*1024**2, storage_pages=8*1024**2)
SHIPPING = {
    'graph-core-arm64': ['graph-cypher'],
    'graph-ffi-arm64': ['graph-cypher'],
    'graph-c-static-arm64': ['graph-cypher'],
    'graph-c-dylib-arm64': ['graph-cypher'],
    'graph-swift-arm64': ['graph-cypher'],
    'legacy-core-arm64': [], 'legacy-core-x86_64': [], 'legacy-core-windows': [],
    'legacy-ffi-arm64': [], 'legacy-ffi-x86_64': [], 'legacy-ffi-windows': [],
    'node-macos-arm64': ['graph-cypher'], 'node-macos-x86_64': ['graph-cypher'],
    'node-windows': ['graph-cypher'], 'text-arm64': [], 'legacy-consumer': [],
}
STATES = ('planned', 'source-inspected', 'compiled', 'RED-observed',
          'focused-GREEN', 'qualified', 'blocked', 'excluded')


def required_cells():
    """Reviewed matrix; input manifests cannot reduce these requirements."""
    cells = {name: 'ZE-118' for name in ('coverage', 'dependency', 'fmt-clippy',
             'header', 'layout', 'ownership', 'registry', 'asan', 'tsan', 'miri',
             'legacy/core', 'legacy/text', 'legacy/vector', 'legacy/python', 'legacy/node')}
    cells.update({'faults': 'ZE-75', 'profile': 'ZE-74', 'shipping': 'ZE-71/ZE-108/ZE-287',
                  'durable-write-history': 'ZE-290', 'acceptance-record': 'ZE-29',
                  'retained-research': 'research/graph/qualification-contracts.md'})
    for surface in SURFACES:
        cells[f'parity/{surface}'] = 'ZE-278/ZE-74' if surface == 'swift' else 'ZE-74'
        cells[f'resources/{surface}'] = 'ZE-76'
        for path in ('structured', 'cypher'):
            for state in ('A', 'B'):
                for shape in SHAPES:
                    cells[f'workload/{surface}/{path}/{state}/{shape}'] = 'ZE-77'
        for name in ('structured-import', 'cypher-meeting-metadata', 'stress-limits',
                     'mixed-load', 'bounded-tail-recovery', 'retention-churn', 'quality'):
            cells[f'workload/{surface}/{name}'] = 'ZE-77/ZE-76'
    for kind in ('c-static', 'c-dylib', 'swift', 'rust'):
        cells[f'installed/{kind}'] = 'ZE-71/ZE-278'
    cells['runtime/macos14-arm64'] = 'ZE-71'
    for artifact in SHIPPING:
        cells[f'footprint/{artifact}'] = 'ZE-287' if 'x86_64' in artifact else 'ZE-71/ZE-108'
    return cells


def inventory():
    """Source-inspected inventory; this is never a shipping measurement."""
    workspace = tomllib.loads((ROOT / 'Cargo.toml').read_text())['workspace']
    crates = {}
    for directory in workspace['members']:
        package = tomllib.loads((ROOT / directory / 'Cargo.toml').read_text())
        crates[package['package']['name']] = dict(path=directory, features=package.get('features', {}))
    return dict(workspace=crates, shipping=SHIPPING,
                node_build='bindings/node/scripts/build-native.mjs',
                profile_sha256=digest(ROOT / 'crates/zeppelin-embed-cypher/tests/fixtures/cypher-profile-v1.json'))


def profile_scenarios():
    raw = json.loads((ROOT / 'crates/zeppelin-embed-cypher/tests/fixtures/cypher-profile-v1.json').read_text())
    return {row['coordinate']: dict(fixture_block_sha256=row['fixture_block_sha256'],
                                   support_disposition=row['support_disposition'])
            for row in raw['scenarios']}


def digest(path):
    return hashlib.sha256(path.read_bytes()).hexdigest()


def current_identity():
    source = subprocess.check_output(['git', 'rev-parse', 'HEAD'], cwd=ROOT, text=True).strip()
    paths = subprocess.check_output(['git', 'ls-files', '-z', '--cached', '--others',
                                     '--exclude-standard'], cwd=ROOT).decode().split('\0')
    build = hashlib.sha256()
    for name in sorted(set(paths) - {''}):
        path = ROOT / name
        build.update(name.encode() + b'\0')
        build.update(path.read_bytes() if path.is_file() else b'<deleted>')
    return dict(source=source, build=build.hexdigest())


def template(identity):
    return dict(schema=1, identity=identity, inventory=inventory(), cells={name: dict(identity=dict(identity),
                state='blocked', threshold=policy(name), missing_input=owner) for name, owner in required_cells().items()})


def reference(ref, base):
    if not isinstance(ref, dict) or set(ref) != {'path', 'sha256'}:
        raise ValueError('missing path/digest reference')
    path = base / ref['path']
    if not path.is_file():
        raise ValueError(f'missing retained evidence: {path}')
    if digest(path) != ref['sha256']:
        raise ValueError(f'tampered digest: {path}')
    return path


def number(value):
    return isinstance(value, (int, float)) and not isinstance(value, bool) and math.isfinite(value)


def check_crate_lines(report, required=CRATES):
    errors = []
    crates = report.get('crates', {})
    for name in required:
        lines = crates.get(name, {})
        covered, total = lines.get('covered'), lines.get('count')
        if not number(total) or total <= 0 or not number(covered) or not 0 <= covered <= total:
            errors.append(f'{name}: missing/unmeasured line coverage')
        elif covered * 100 < total * 90:
            errors.append(f'{name}: {100*covered/total:.2f}% lines below 90%')
    if report.get('exclusions') != EXCLUSIONS:
        errors.append('unrecorded coverage exclusions')
    return errors


def coverage_report(paths):
    """Adapt llvm-cov export JSON, retaining benchmark exclusions explicitly.

    Reports may be split (workspace and benchmark). Repeated files must agree;
    never add duplicate instantiations or average percentages across crates.
    """
    files = {}
    for path in paths:
        raw = json.loads(path.read_text())
        for data in raw['data']:
            for item in data['files']:
                filename = item['filename'].replace('\\', '/')
                lines = item['summary']['lines']
                if filename in files and files[filename] != lines:
                    raise ValueError(f'conflicting coverage for {filename}')
                files[filename] = lines
    crates = {name: dict(count=0, covered=0) for name in CRATES}
    for filename, lines in files.items():
        relative = filename.removeprefix(str(ROOT) + '/')
        if any(relative.startswith(exclusion) for exclusion in EXCLUSIONS):
            continue
        # Longest path wins: tests/adversarial-oracle is not workspace-tests.
        for name, directory in sorted(CRATES.items(), key=lambda pair: -len(pair[1])):
            if relative.startswith(directory + '/'):
                for key in ('count', 'covered'):
                    crates[name][key] += lines[key]
                break
    return dict(crates=crates, exclusions=EXCLUSIONS)


def section_bytes(raw):
    """Platform size output: count linkable sections, excluding LLVM bitcode."""
    total = 0
    for line in raw.splitlines():
        match = re.match(r'\s*Section (?!\(__LLVM,).*?\s(\d+)(?: \(zerofill\))?$', line)
        if match:
            total += int(match.group(1))
        else:
            match = re.match(r'\s*(\.[^\s]+)\s+(\d+)\s+', line)
            if match and not match.group(1).startswith('.llvm'):
                total += int(match.group(2))
    if total <= 0:
        raise ValueError('unmeasured linkable sections')
    return total


def artifact_report(raw, artifact, reachability):
    """Adapt check-graph-artifact.measure / size-budget raw measurements.

    Reachability comes from a separately executed consumer receipt; measurements
    alone cannot assert that an installed structured/string path executed.
    """
    if artifact not in SHIPPING:
        raise ValueError('unreviewed shipping artifact: ' + artifact)
    return dict(raw, **{'pass': raw.get('exit_status') == 0, 'features': SHIPPING[artifact],
                       'reachability': reachability})


def installed_report(raw, kind):
    receipt = raw.get('reachability', {}).get(kind, {})
    required = ['batch', 'structured', 'get', 'cypher']
    if receipt.get('executed') != required or receipt.get('artifact_kind') != 'graph-cypher':
        raise ValueError('missing installed structured query/get execution: ZE-71/ZE-278; '
                         'ze_graph_query exists, but exports or batch/Cypher success are insufficient')
    return dict(receipt, **{'pass': receipt.get('exit_status') == 0,
        'checksum_verified': raw.get('contract', {}).get('matches') is True,
        'legacy_substitution_rejected': bool(raw.get('legacy_substitution'))})


def footprint_budget(artifact):
    # Owner decisions: 2026-09-27 graph FFI 12,288 KiB (CLAUDE.md);
    # 2026-10-05 legacy 5,632 KiB on all platforms. Graph-contained core
    # is reported separately; it receives no independent archive gate.
    if artifact in ('graph-core-arm64', 'text-arm64', 'legacy-consumer'):
        return None
    return (12288 if SHIPPING[artifact] else 5632) * 1024


def policy(name):
    """Frozen applicable thresholds appear in every submitted receipt."""
    if name == 'coverage':
        return {'per_crate_lines_percent_min': 90, 'crates': list(CRATES)}
    if name.startswith('footprint/'):
        return {'section_bytes_max': footprint_budget(name.split('/')[1])}
    if name.startswith('resources/'):
        return {'managed_bytes_max': 256*1024**2, 'limits': LIMITS}
    if name.startswith('workload/'):
        return {'valid_repetitions': 5, 'managed_bytes_max': 256*1024**2,
                'recall_at_20_min': .95,
                'p95_ms_max': 2000 if name.endswith('bounded-tail-recovery') else (
                    None if name.endswith(('quality', 'retention-churn', 'stress-limits', 'mixed-load')) else 250)}
    return {}


def check_shipping_features(report):
    errors = []
    observed = report.get('artifacts', [])
    identities = [item.get('id') for item in observed]
    if len(set(identities)) != len(identities) or set(identities) != set(SHIPPING):
        errors.append('shipping artifact inventory missing, duplicate or unreviewed artifact (ZE-108)')
    for item in observed:
        name = item.get('id')
        if name not in SHIPPING or item.get('features') != SHIPPING[name]:
            errors.append(f'shipping {name}: omitted/unreviewed feature')
        if item.get('exports_checked') is not True or item.get('headers_checked') is not True:
            errors.append(f'shipping {name}: exports/headers unverified')
        if item.get('default_graph_absent') is not True:
            errors.append(f'shipping {name}: default feature isolation unverified')
        if not item.get('target') or not item.get('sha256'):
            errors.append(f'shipping {name}: missing target/artifact identity')
    return errors


def observations_errors(name, obs):
    errors = []
    def require(key, expected=True):
        if obs.get(key) != expected:
            errors.append(f'{key}: required {expected!r}')
    def measured(key, maximum=None, minimum=0):
        value = obs.get(key)
        if not number(value) or value < minimum or (maximum is not None and value > maximum):
            errors.append(f'{key}: missing/unmeasured or outside [{minimum}, {maximum}]')
    require('pass')
    require('exit_status', 0)
    if name == 'coverage':
        errors.extend(check_crate_lines(obs))
    elif name == 'shipping':
        errors.extend(check_shipping_features(obs))
    elif name.startswith('footprint/'):
        measured('section_bytes', footprint_budget(name.split('/')[1]), 1)
        measured('physical_bytes', minimum=1)
        require('features', SHIPPING[name.split('/')[1]])
        require('reachability', ['structured', 'cypher'] if SHIPPING[name.split('/')[1]] else ['legacy'])
    elif name.startswith('installed/'):
        require('executed', ['batch', 'structured', 'get', 'cypher'])
        require('artifact_kind', 'graph-cypher')
        require('checksum_verified')
        require('legacy_substitution_rejected')
    elif name == 'runtime/macos14-arm64':
        require('architecture', 'arm64')
        require('os_major', 14)
        require('executed', ['rust', 'c-static', 'c-dylib', 'swift'])
    elif name == 'faults':
        pairs = obs.get('pairs', [])
        if {p.get('site') for p in pairs} != set(FAULTS) or len(pairs) != len(FAULTS):
            errors.append('incomplete fault/control surface inventory')
        for pair in pairs:
            if (not number(pair.get('fired')) or pair['fired'] <= 0
                    or pair.get('fault_pass') is not True or pair.get('control_pass') is not True
                    or pair.get('seed') != pair.get('control_seed')
                    or not pair.get('comparator') or pair.get('negative_control_rejected') is not True):
                errors.append('incomplete fault/control pair or unfired fault')
    elif name.startswith('resources/'):
        measured('peak_bytes', 256*1024**2, 1)
        require('readers', 4)
        require('writers', 1)
        require('lying_counter_rejected')
        require('ownership_released')
        require('boundaries', {key: dict(cap=value, at_cap=True, one_over_rejected=True)
                               for key, value in LIMITS.items()})
        for key in ('canonical_live_bytes', 'canonical_retired_bytes', 'fence_bytes',
                    'fence_entries', 'mapped_bytes', 'mapped_resident_bytes', 'wal_bytes',
                    'disk_bytes', 'phys_footprint', 'caller_bytes', 'retained_result_bytes'):
            measured(key)
    elif name.startswith('workload/'):
        require('independent_oracle')
        require('full_domain')
        repetitions = obs.get('repetitions', [])
        valid = []
        for rep in repetitions:
            if rep.get('failed') is not False:
                errors.append('failed repetition cannot be discarded as taint')
            if rep.get('tainted') is False:
                valid.append(rep)
                monitor = rep.get('monitor', [])
                if not monitor or rep.get('monitor_complete') is not True or rep.get('digests_stable') is not True:
                    errors.append('incomplete monitor/taint controls')
                else:
                    previous_cpu = 0
                    baseline = tuple(monitor[0].get(key) for key in ('power', 'ac', 'qos'))
                    for index, sample in enumerate(monitor):
                        cpu = sample.get('nonbenchmark_cpu_percent')
                        if (sample.get('second') != index or sample.get('thermal') != 'nominal'
                                or any(value is None for value in baseline)
                                or tuple(sample.get(key) for key in ('power', 'ac', 'qos')) != baseline
                                or not number(cpu) or not 0 <= cpu <= 100
                                or (cpu > 10 and previous_cpu > 10)):
                            errors.append('tainted/incomplete monitor in valid repetition')
                        previous_cpu = cpu if number(cpu) else 0
                    if not number(rep.get('duration_seconds')) or len(monitor) != math.ceil(rep['duration_seconds']) + 1:
                        errors.append('missing one-second monitor samples')
        if len(valid) != 5:
            errors.append('requires five valid complete repetitions (ZE-77)')
        for rep in valid:
            samples = rep.get('samples_ms', [])
            if (not samples or len(samples) != rep.get('samples')
                    or any(not number(value) or value < 0 for value in samples)):
                errors.append('missing/incomplete raw latency samples/p95')
            elif sorted(samples)[math.ceil(.95*len(samples))-1] != rep.get('p95_ms'):
                errors.append('reported p95 differs from raw samples')
            if not number(rep.get('p95_ms')) or rep['p95_ms'] < 0:
                errors.append('unmeasured p95')
            else:
                limit = 2000 if name.endswith('bounded-tail-recovery') else 250
                if not name.endswith(('quality', 'retention-churn', 'stress-limits', 'mixed-load')) and rep['p95_ms'] > limit:
                    errors.append(f'p95 exceeds {limit} ms')
            if not number(rep.get('peak_bytes')) or not 0 < rep['peak_bytes'] <= 256*1024**2:
                errors.append('unmeasured/excess managed peak')
            minimum = 20 if name.endswith('bounded-tail-recovery') else (200 if name.endswith(('structured-import', 'cypher-meeting-metadata')) else 1000)
            if not number(rep.get('samples')) or rep['samples'] < minimum:
                errors.append('incomplete sample schedule')
        if 'lexical-evidence' in name:
            require('exact_scores_match')
        if name.endswith('quality') or any(shape in name for shape in ('semantic-context', 'alice-project-ranking', 'hybrid-project-evidence')):
            measured('recall_at_20', 1, .95)
            require('exact_scores_match')
        if name.endswith('retention-churn'):
            require('history_multipliers', [1, 5, 10])
        if name.endswith('stress-limits'):
            require('stress_multiplier', 10)
            require('hops', [1, 2, 4, 8, 16])
            require('limit_failures_separate')
        if name.endswith('mixed-load'):
            require('readers', 4)
            require('writers', 1)
            require('synchronized_start')
        if name.endswith(('structured-import', 'cypher-meeting-metadata')):
            require('baseline_ingestion_batches', 9204)
            require('checkpoint_stalls_included')
        if name.endswith('bounded-tail-recovery'):
            require('tail_thresholds', ['64-envelopes', '16-MiB'])
            require('read_only_zero_writes')
    elif name == 'durable-write-history':
        measured('ingestion_batches', minimum=9204)
        require('checkpoint_reopen')
        require('unchanged_state_rejection')
    elif name.startswith('parity/') or name == 'profile':
        require('common_fixture')
        require('full_width_ids')
        require('unsupported_rejections')
        require('side_effects_match')
        if name == 'profile':
            scenarios = obs.get('scenarios', {})
            expected = profile_scenarios()
            if set(scenarios) != set(expected):
                errors.append('missing/extra profile scenarios (ZE-74)')
            for coordinate, record in expected.items():
                observed = scenarios.get(coordinate, {})
                if any(observed.get(key) != value for key, value in record.items()):
                    errors.append(f'stale profile scenario {coordinate}')
                state = 'executed-pass' if record['support_disposition'] == 'supported' else 'unsupported-rejected'
                if observed.get('state') != state:
                    errors.append(f'unexecuted/failed profile scenario {coordinate}')
    return errors


def validate_evidence(name, cell, base, identity, decisions):
    if cell.get('state') == 'excluded':
        decision = decisions.get(cell.get('decision'), {})
        try:
            reference(decision['record'], base)
            if name not in decision['cells'] or not decision['owner'] or not cell.get('reason'):
                raise ValueError('decision does not authorize cell')
            return []
        except (KeyError, ValueError, TypeError):
            return ['exclusion requires recorded owner decision from --decisions']
    errors = []
    if cell.get('threshold') != policy(name):
        errors.append('missing/changed applicable threshold')
    if cell.get('identity') != identity:
        errors.append('stale source/build identity')
    if cell.get('state') != 'qualified':
        errors.append(f"state {cell.get('state')!r} is not qualified; missing input {required_cells()[name]}")
        return errors
    if cell.get('exit_status') != 0 or isinstance(cell.get('exit_status'), bool):
        errors.append('missing/failed exit status')
    for field in ('command', 'target', 'toolchain', 'fixture', 'features'):
        if field not in cell or (field != 'features' and not cell[field]):
            errors.append(f'missing {field}')
    if not isinstance(cell.get('fixture'), str) or not re.fullmatch(r'[0-9a-f]{64}', cell['fixture']):
        errors.append('missing/invalid fixture digest')
    if not isinstance(cell.get('command'), list) or not all(isinstance(arg, str) and arg for arg in cell.get('command', [])):
        errors.append('command must be a nonempty argument array')
    if not isinstance(cell.get('features'), list) or not all(isinstance(feature, str) for feature in cell.get('features', [])):
        errors.append('features must be an explicit array')
    if 'seed' not in cell:
        errors.append('missing seed')
    try:
        report = json.loads(reference(cell['report'], base).read_text())
        if not isinstance(report, dict):
            raise ValueError('report observations must be an object')
        reference(cell['log'], base)
        if not cell['artifacts']:
            raise ValueError('missing retained artifact')
        for ref in cell['artifacts']:
            reference(ref, base)
        if report != cell.get('observations'):
            raise ValueError('observations differ from retained report')
        if name == 'shipping':
            retained = {ref['sha256'] for ref in cell['artifacts']}
            for item in report.get('artifacts', []):
                if item.get('sha256') not in retained:
                    errors.append('shipping artifact digest has no matching retained bytes')
        if name.startswith('footprint/'):
            matching = [ref for ref in cell['artifacts'] if ref['sha256'] == report.get('sha256')]
            if not matching:
                errors.append('footprint artifact digest has no matching retained bytes')
            elif reference(matching[0], base).stat().st_size != report.get('physical_bytes'):
                errors.append('physical artifact size differs from retained bytes')
            size_raw = reference(dict(path=report['raw_size'], sha256=report['raw_sha256']), base)
            if section_bytes(size_raw.read_text()) != report.get('section_bytes'):
                errors.append('section bytes differ from retained raw size output')
        errors.extend(observations_errors(name, report))
    except (KeyError, ValueError, TypeError, OSError) as error:
        errors.append(str(error))
    return errors


def qualify(manifest, base, identity, decisions):
    errors = []
    observed_inventory = inventory()
    if set(observed_inventory['workspace']) != set(CRATES):
        errors.append('workspace inventory changed: review every crate/frontend before qualification')
    if manifest.get('inventory') != observed_inventory:
        errors.append('omitted/stale workspace, frontend or shipping inventory')
    if manifest.get('schema') != 1:
        errors.append('unsupported manifest schema')
    if manifest.get('identity') != identity:
        errors.append('stale manifest source/build identity')
    cells = manifest.get('cells', {})
    if not isinstance(cells, dict):
        return errors + ['cells must be a keyed evidence object']
    for name, owner in required_cells().items():
        if name not in cells:
            errors.append(f'{name}: missing required cell; input {owner}')
        elif not isinstance(cells[name], dict):
            errors.append(f'{name}: cell must be an object')
        else:
            errors.extend(f'{name}: {error}' for error in validate_evidence(name, cells[name], base, identity, decisions))
    for name in cells.keys() - required_cells().keys():
        errors.append(f'{name}: unreviewed cell')
    return errors


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--inventory', action='store_true', help='print source-inspected feature/artifact inventory; not qualification')
    parser.add_argument('--schema', action='store_true', help='print receipt schema and frozen cell thresholds')
    parser.add_argument('--manifest', type=Path)
    parser.add_argument('--results', type=Path)
    parser.add_argument('--init', action='store_true', help='write blocked manifest; never qualify')
    parser.add_argument('--decisions', type=Path, help='reviewed owner decisions, separate from submitted manifest')
    parser.add_argument('--coverage-json', type=Path, nargs='+', help='adapt retained llvm-cov exports; no campaign')
    parser.add_argument('--coverage-output', type=Path)
    parser.add_argument('--coverage-crates', nargs='+', choices=tuple(CRATES), help='routine platform-selected coverage only; release still requires all seven')
    parser.add_argument('--attach', choices=tuple(required_cells()), help='attach one retained producer receipt; still validate full matrix')
    parser.add_argument('--report', type=Path)
    parser.add_argument('--log', type=Path)
    parser.add_argument('--artifacts', type=Path, nargs='+')
    parser.add_argument('--target')
    parser.add_argument('--features', nargs='*', default=[])
    parser.add_argument('--fixture')
    parser.add_argument('--seed', type=int)
    parser.add_argument('--toolchain')
    parser.add_argument('--state', choices=STATES, default='focused-GREEN')
    parser.add_argument('--adapter', choices=('normalized', 'installed', 'artifact'), default='normalized')
    parser.add_argument('--reachability', nargs='+', default=[])
    parser.add_argument('--command', nargs='+')
    args = parser.parse_args()
    if args.inventory:
        print(json.dumps(inventory(), indent=2))
        return 0
    if args.schema:
        print(json.dumps(dict(version=1, states=STATES,
            manifest_fields=['schema', 'identity', 'inventory', 'cells'],
            receipt_fields=['identity', 'state', 'threshold', 'command', 'exit_status', 'target',
                'features', 'toolchain', 'fixture', 'seed', 'report', 'log', 'artifacts', 'observations'],
            reference_fields=['path', 'sha256'],
            required_cells={name: dict(input=owner, threshold=policy(name)) for name, owner in required_cells().items()}), indent=2))
        return 0
    if args.coverage_json:
        report = coverage_report(args.coverage_json)
        if args.coverage_output:
            args.coverage_output.write_text(json.dumps(report, indent=2) + '\n')
        errors = check_crate_lines(report, args.coverage_crates or CRATES)
    else:
        if not args.manifest:
            parser.error('--manifest is required')
        identity = current_identity()
        if args.init:
            args.manifest.parent.mkdir(parents=True, exist_ok=True)
            args.manifest.write_text(json.dumps(template(identity), indent=2) + '\n')
        manifest = json.loads(args.manifest.read_text())
        if args.attach:
            if not all((args.report, args.log, args.artifacts, args.target, args.fixture,
                        args.toolchain, args.command)) or args.seed is None:
                parser.error('--attach requires report, log, artifacts, target, fixture, seed, toolchain, command')
            observations = json.loads(args.report.read_text())
            if args.adapter == 'installed':
                observations = installed_report(observations, args.attach.removeprefix('installed/'))
            elif args.adapter == 'artifact':
                observations = artifact_report(observations, args.attach.removeprefix('footprint/'), args.reachability)
            normalized = args.manifest.with_name(args.attach.replace('/', '_') + '-report.json')
            if normalized.resolve() == args.report.resolve():
                parser.error('normalized output must not overwrite input report')
            normalized.write_text(json.dumps(observations, indent=2) + '\n')
            def ref(path):
                return dict(path=str(path.resolve()), sha256=digest(path))
            manifest['cells'][args.attach] = dict(identity=identity, state=args.state,
                threshold=policy(args.attach), command=args.command, exit_status=observations.get('exit_status'),
                target=args.target, features=args.features, fixture=args.fixture, seed=args.seed,
                toolchain=args.toolchain, report=ref(normalized), log=ref(args.log),
                artifacts=[ref(path) for path in [args.report, *args.artifacts]], observations=observations)
            args.manifest.write_text(json.dumps(manifest, indent=2) + '\n')
        decisions = json.loads(args.decisions.read_text()) if args.decisions else {}
        errors = qualify(manifest, args.manifest.parent, identity, decisions)
        results = args.results or args.manifest.with_name('RESULTS.md')
        results.write_text('# Graph release evidence\n\n' + ('BLOCKED — not qualified\n' if errors else 'QUALIFIED\n')
                           + '\nSource/build: `' + json.dumps(identity) + '`\n\n'
                           + '\n'.join(f'- {error}' for error in errors) + '\n')
    for error in errors:
        print(error, file=sys.stderr)
    return 1 if errors else 0


if __name__ == '__main__':
    try:
        sys.exit(main())
    except (ValueError, KeyError, TypeError, OSError) as error:
        print(f'graph qualification failed: {error}', file=sys.stderr)
        sys.exit(1)
