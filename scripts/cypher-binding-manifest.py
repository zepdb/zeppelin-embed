#!/usr/bin/env python3
"""Verify the pinned, tooling-only Cypher support/binding/execution manifest.

Original queries and expectations remain in openCypher; hashes link them exactly.
Binding is independently evidenced by the named Rust fixture test. Execution is
unexecuted here, including selected scenarios with expected runtime errors.
"""
import argparse
import hashlib
import json
from pathlib import Path
import re
import subprocess

PIN = '007895aff5f33097d67b2e48a0a2babd6bd18590'
ROOT = Path(__file__).resolve().parents[1]
FIXTURE = ROOT / 'crates/zeppelin-embed-cypher/tests/fixtures/selected-tck-syntax.txt'
OUTPUT = ROOT / 'crates/zeppelin-embed-cypher/tests/fixtures/binding-manifest.json'
REJECTED = {
    'clauses/match/Match4.feature': [2, 5, 7, 8],
    'clauses/match/Match7.feature': [12, 20],
    'clauses/with/With6.feature': [4, 5, 6, 7],
    'expressions/aggregation/Aggregation8.feature': [3, 4],
    'clauses/return/Return5.feature': [1, 3, 4],
    'expressions/null/Null1.feature': [5],
    'expressions/list/List1.feature': [5],
    'clauses/remove/Remove1.feature': [2, 4, 7],
}

def digest(data):
    return hashlib.sha256(data).hexdigest()

def generate(checkout):
    head = subprocess.check_output(['git', '-C', str(checkout), 'rev-parse', 'HEAD'], text=True).strip()
    if head != PIN:
        raise ValueError(f'expected source {PIN}, got {head}')
    fixture = FIXTURE.read_bytes()
    selected = {}
    for entry in fixture.decode().split('\n---\n')[1:]:
        label, query = entry.split('\n', 1)
        match = re.fullmatch(r'(parse|reject) (.+\.feature) \[(\d+)\] (setup|query)', label)
        if not match:
            raise ValueError(f'invalid fixture label: {label}')
        _, feature, scenario, phase = match.groups()
        selected.setdefault((feature, int(scenario)), []).append({
            'phase': phase, 'query_sha256': digest(query.rstrip('\n').encode()),
            'expected_binding': 'reject' if label.startswith('reject ') or (feature == 'clauses/match/Match3.feature' and scenario == '29' and phase == 'query') else 'bind',
        })
    if len(selected) != 99 or sum(map(len, selected.values())) != 155:
        raise ValueError('selected inventory must remain exactly 99 scenarios / 155 statements')
    rejected = {(feature, number) for feature, numbers in REJECTED.items() for number in numbers}
    features = sorted({feature for feature, _ in selected} | set(REJECTED))
    if len(features) != 26 or set(selected) & rejected:
        raise ValueError('profile inventory overlap or unexpected feature count')
    rows = []
    seen = set()
    sources = []
    for feature in features:
        path = f'tck/features/{feature}'
        data = (checkout / path).read_bytes()
        pinned = subprocess.check_output(['git', '-C', str(checkout), 'show', f'{PIN}:{path}'])
        if data != pinned:
            raise ValueError(f'working source differs from pinned object: {path}')
        sources.append({'path': path, 'sha256': digest(data)})
        source = data.decode()
        starts = list(re.finditer(r'^  Scenario(?: Outline)?: \[(\d+)\] (.+)$', source, re.M))
        for index, match in enumerate(starts):
            number = int(match.group(1))
            key = (feature, number)
            if key in seen:
                raise ValueError(f'duplicate scenario coordinate: {key}')
            seen.add(key)
            end = starts[index + 1].start() if index + 1 < len(starts) else len(source)
            block = source[match.start():end]
            disposition = 'supported' if key in selected else 'rejected_profile' if key in rejected else 'not_selected'
            row = {
                'feature': feature, 'scenario': number, 'title': match.group(2),
                'source_block_sha256': digest(block.encode()),
                'support_disposition': disposition,
                'binding': {'state': 'unexecuted'},
                'execution': {'state': 'unexecuted', 'evidence': None},
            }
            if key in selected:
                original_queries = []
                for query in re.finditer(r'(And having executed|When executing query):\n\s+"""\n(.*?)\n\s+"""', block, re.S):
                    original_queries.append({
                        'phase': 'setup' if query.group(1).startswith('And') else 'query',
                        'query_sha256': digest(query.group(2).encode()),
                    })
                actual_queries = [{name: item[name] for name in ('phase', 'query_sha256')} for item in selected[key]]
                if actual_queries != original_queries:
                    raise ValueError(f'fixture differs from original query: {key}')
                row['binding'] = {
                    'state': 'passing',
                    'evidence': 'selected_original_statements_bind_without_claiming_execution',
                    'statements': selected[key],
                }
                expected = re.search(r'Then a (\w+) should be raised at (compile time|runtime): (\w+)', block)
                if expected:
                    row['original_expected_error'] = {'class': expected.group(1), 'phase': expected.group(2).split()[0], 'detail': expected.group(3)}
            rows.append(row)
    if not (set(selected) | rejected) <= seen:
        raise ValueError('required scenario coordinate missing')
    errors = [row['original_expected_error'] for row in rows if 'original_expected_error' in row]
    if len(errors) != 4 or sum(error['phase'] == 'runtime' for error in errors) != 1:
        raise ValueError('preserve exactly three original compile errors and one runtime error')
    return {
        'format': 'zeppelin-cypher-binding-manifest-v1',
        'source_commit': PIN,
        'source_license': 'Apache-2.0; original notices and source blocks remain at the pinned source',
        'profile': 'documented openCypher 9 subset with Zeppelin search extensions',
        'scope': 'All scenarios in the exact 26 selected feature files; not the complete openCypher corpus',
        'execution_status': 'unexecuted; binding success is not result, side-effect, or TCK conformance evidence',
        'binding_fixture_sha256': digest(fixture),
        'selected_scenarios': 99,
        'binding_statements': {'bind': 152, 'reject': 3},
        'sources': sources,
        'scenarios': rows,
    }

def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('checkout', type=Path)
    parser.add_argument('--write', action='store_true')
    args = parser.parse_args()
    data = (json.dumps(generate(args.checkout), indent=2, ensure_ascii=False) + '\n').encode()
    if args.write:
        OUTPUT.write_bytes(data)
    elif OUTPUT.read_bytes() != data:
        raise ValueError('manifest differs; inspect source then regenerate explicitly')
    print(f'verified {digest(data)}; 99 selected scenarios, 152 bound + 3 compile rejections, zero executed')

if __name__ == '__main__':
    main()
