#!/usr/bin/env python3
"""Frozen corpus, public execution receipts and fail-closed source verification.

JSON and SHA-256 belong to tooling; the Rust runner has no added dependencies.
"""
import argparse
import hashlib
import importlib.util
import json
import os
from pathlib import Path
import re
import subprocess
import textwrap

ROOT = Path(__file__).resolve().parents[1]
FIXTURES = ROOT / 'crates/zeppelin-embed-cypher/tests/fixtures'
OUTPUT = FIXTURES / 'cypher-profile-v1.json'
PIN = '007895aff5f33097d67b2e48a0a2babd6bd18590'

def digest(value):
    return hashlib.sha256(value.encode() if isinstance(value, str) else value).hexdigest()

def fixture_sections(body):
    sections = {'setup': [], 'parameter': [], 'query': [], 'expectation': [], 'side_effects': []}
    section = None
    for line in body.splitlines():
        if line == 'setup':
            section = 'setup'
            sections[section].append('setup')
        elif line == 'query':
            section = 'query'
        elif line.startswith('parameter '):
            sections['parameter'].append(line)
        elif line.startswith('expect ') or line.startswith('result '):
            section = 'expectation'
            sections[section].append(line)
        elif line.startswith('side-effects'):
            section = 'side_effects'
            sections[section].append(line)
        else:
            if section is None:
                raise ValueError('unknown fixture section')
            sections[section].append(line)
    return sections

def corpus():
    rows = []
    for name in ('read', 'write'):
        fixture = f'{name}-tck-execution.txt'
        for block in (FIXTURES / fixture).read_text().split('\n=== ')[1:]:
            coordinate, body = block.split('\n', 1)
            sections = fixture_sections(body)
            expectation = '\n'.join(sections['expectation'])
            disposition = ('rejected_profile' if expectation.startswith('expect reject-profile') else
                           'local_observation' if expectation.startswith('expect local-example') else 'supported')
            rows.append({'coordinate': coordinate, 'fixture': fixture,
                         'fixture_block_sha256': digest(body),
                         'hashes': {key: digest('\n'.join(value)) for key, value in sections.items()},
                         'support_disposition': disposition, 'execution': {'state': 'unexecuted', 'history': []}})
    if len(rows) != 130 or len({r['coordinate'] for r in rows}) != 130:
        raise ValueError('corpus must contain 130 unique execution coordinates')
    if sum(r['support_disposition'] == 'supported' for r in rows) != 99:
        raise ValueError('corpus must retain 99 originals')
    return rows

def verify_sources(checkout):
    spec = importlib.util.spec_from_file_location('binding', ROOT / 'scripts/cypher-binding-manifest.py')
    binding = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(binding)
    manifest = binding.generate(checkout)
    conventions = {}
    for path in ('tck/README.adoc', 'tools/tck-api/src/main/scala/org/opencypher/tools/tck/api/Result.scala'):
        data = (checkout / path).read_bytes()
        pinned = subprocess.check_output(['git', '-C', str(checkout), 'show', f'{PIN}:{path}'])
        if data != pinned:
            raise ValueError(f'stale conventions: {path}')
        conventions[path] = digest(data)
    # Retain full original bodies, including rejected expectations, independently
    # of the Zeppelin execution fixture's expected profile refusal.
    sources = {}
    for row in manifest['scenarios']:
        if row['support_disposition'] == 'not_selected':
            continue
        feature = row['feature']
        source = (checkout / 'tck/features' / feature).read_text()
        starts = list(re.finditer(r'^  Scenario(?: Outline)?: \[(\d+)\] (.+)$', source, re.M))
        for i, match in enumerate(starts):
            if int(match[1]) != row['scenario']:
                continue
            block = source[match.start():starts[i+1].start() if i+1 < len(starts) else len(source)]
            sources[f'{feature} [{row["scenario"]}]'] = {'title': match[2], 'original_body': block,
                                                       'sha256': digest(block)}
    verify_expectations(sources)
    return manifest, conventions, sources

def verify_expectations(sources):
    """Compare original setup/query/parameters/results/errors/effects, not only parsing."""
    def query(text):
        return textwrap.dedent(text).strip()
    def cells(lines):
        return [[cell.strip() for cell in line.strip().strip('|').split('|')]
                for line in lines if line.strip().startswith('|')]
    for name in ('read', 'write'):
        for block in (FIXTURES / f'{name}-tck-execution.txt').read_text().split('\n=== ')[1:]:
            coordinate, body = block.split('\n', 1)
            parts = fixture_sections(body)
            if parts['expectation'][0].startswith(('expect reject-profile', 'expect local-example')):
                continue  # original rejected body retained independently, never a semantic pass
            original = sources[coordinate]['original_body']
            queries = list(re.finditer(r'(And having executed|When executing query):\n\s+"""\n(.*?)\n\s+"""', original, re.S))
            actual_setups = '\n'.join(parts['setup']).split('setup\n')[1:]
            original_setups = [m[2] for m in queries if m[1].startswith('And')]
            original_queries = [m[2] for m in queries if m[1].startswith('When')]
            if list(map(query, actual_setups)) != list(map(query, original_setups)) or [query('\n'.join(parts['query']))] != list(map(query, original_queries)):
                raise ValueError(f'altered original query/setup: {coordinate}')
            parameter = re.search(r'And parameters are:(.*?)(?=\n    When)', original, re.S)
            actual_parameters = [line[len('parameter '):].split(' = ', 1) for line in parts['parameter']]
            if actual_parameters != cells(parameter[1].splitlines() if parameter else []):
                raise ValueError(f'altered original parameters: {coordinate}')
            expectation = parts['expectation'][0]
            error = re.search(r'Then a (\w+) should be raised at (compile time|runtime): (\w+)', original)
            if error:
                expected = 'expect ' + ('compile-error' if error[2] == 'compile time' else 'runtime-error') + f' {error[1]} {error[3]}'
                if expectation != expected:
                    raise ValueError(f'altered original error: {coordinate}')
            elif expectation == 'expect empty':
                if 'Then the result should be empty' not in original:
                    raise ValueError(f'altered empty expectation: {coordinate}')
            else:
                table = re.search(r'Then the result should be, (.*?):(.*?)(?=\n    And|\Z)', original, re.S)
                mode = {'in any order': 'bag', 'in order': 'ordered',
                        'in any order, ignoring element order for lists': 'bag-lists-unordered'}
                if not table or expectation != 'expect ' + mode.get(table[1], 'unknown') or cells(parts['expectation'][1:]) != cells(table[2].splitlines()):
                    raise ValueError(f'altered original results: {coordinate}')
            effects = re.search(r'And the side effects should be:(.*?)(?=\n  Scenario|\Z)', original, re.S)
            if cells(parts['side_effects']) != cells(effects[1].splitlines() if effects else []):
                raise ValueError(f'altered original effects: {coordinate}')

def merge_receipts(rows, log, command, commit, evidence=None):
    receipts = {}
    for line in log.splitlines():
        if 'ZE59\t' not in line:
            continue
        line = 'ZE59\t' + line.split('ZE59\t', 1)[1]
        _, coordinate, state, block_hash, observation = line.split('\t', 4)
        if coordinate in receipts or state != 'GREEN':
            raise ValueError(f'duplicate or failed receipt: {coordinate}')
        receipts[coordinate] = (block_hash, observation)
    expected = {r['coordinate'] for r in rows}
    if set(receipts) != expected:
        raise ValueError(f'incomplete receipts: missing={sorted(expected-set(receipts))}, extra={sorted(set(receipts)-expected)}')
    for row in rows:
        block_hash, observation = receipts[row['coordinate']]
        # Rust emits the exact fixture block as hexadecimal UTF-8, avoiding
        # an extra hashing dependency and untrusted delimiter escaping.
        if digest(bytes.fromhex(block_hash)) != row['fixture_block_sha256']:
            raise ValueError(f'stale receipt: {row["coordinate"]}')
        event = {'state': 'GREEN', 'command': command, 'commit': commit,
                 'observation': observation, 'log_sha256': digest(log),
                 'evidence': evidence, 'uncommitted_component': True}
        row['execution'] = {'state': 'GREEN', 'history': row['execution']['history'] + [event]}

TEST_TARGETS = ['profile_conformance', 'read_tck_execution', 'write_tck_execution', 'read_oracle_execution', 'write_extension_execution',
                'mixed_revision_execution', 'search_execution', 'search_parity', 'relational_execution']
TEST_NAMES = [
    'ze59_original_corpus_emits_complete_receipts',
    'ze59_inventory_has_positive_and_boundary_evidence',
    'ze59_malformed_and_budget_refusals_are_atomic',
    'ze59_compaction_preserves_observations',
    'ze59_small_mutation_model_matches_progressive_frozen_and_ordered_writes',
    'ze56_tiny_graph_oracle_matches_before_and_after_reopen',
    'ze56_cancelled_statements_are_typed_and_commit_nothing',
    'ze57_local_remove_missing_property_is_noop',
    'ze57_local_remove_multiple_properties_with_projection',
    'ze57_local_set_list_property_then_index_reads_back',
    'ze57_local_mixed_structured_and_cypher_revisions_keep_deleted_key_fence',
    'ze58_text_search_returns_real_rows', 'ze58_eligibility_domains_remain_distinct',
    'ze58_independent_calls_preserve_bags_and_eager_counts',
    'ze58_reports_survive_projection_and_aggregation', 'ze58_modes_and_components_preserve_provenance',
    'ze58_search_results_outlive_close_and_reopen', 'ze58_search_compile_rejections_publish_nothing',
    'ze58_search_write_mixing_publish_nothing', 'ze58_runtime_refusals_return_no_partial_result',
    'ze58_full_u128_ties_remain_ordered', 'ze58_graph_coverage_requires_actual_traversal',
    'ze58_three_application_shapes_match_structured',
    'ze58_modality_values_and_reports_match_independent_plans', 'ze255_numeric_aggregates',
]
COMMAND = ['cargo', 'test', '-p', 'zeppelin-embed-cypher']
for target in TEST_TARGETS:
    COMMAND += ['--test', target]
COMMAND += ['--', '--exact'] + TEST_NAMES + ['--nocapture', '--test-threads=1']

# Inventory rows reference actual execution tests; parser/binder evidence alone
# cannot satisfy this map. The original corpus supplies the pattern/write rows.
INVENTORY = {
    **{name: ('ze59_original_corpus_emits_complete_receipts', 'ze59_inventory_has_positive_and_boundary_evidence')
       for name in ['Statement', 'MATCH', 'Inline properties', 'Bounded paths', 'WHERE',
                    'RETURN / WITH', 'CREATE', 'SET', 'REMOVE', 'DELETE']},
    **{name: ('ze59_inventory_has_positive_and_boundary_evidence', 'ze59_inventory_has_positive_and_boundary_evidence')
       for name in ['Tokens/names', 'Constants', 'Access', 'Boolean/comparison', 'Arithmetic', 'String predicates']},
    'Aggregation': ('ze255_numeric_aggregates', 'ze59_inventory_has_positive_and_boundary_evidence'),
    'Functions': ('ze59_original_corpus_emits_complete_receipts', 'ze59_inventory_has_positive_and_boundary_evidence'),
    'Parameters': ('ze59_original_corpus_emits_complete_receipts', 'ze59_inventory_has_positive_and_boundary_evidence'),
    'CALL': ('ze58_independent_calls_preserve_bags_and_eager_counts', 'ze58_search_compile_rejections_publish_nothing'),
    'Stored text (extension)': ('ze58_three_application_shapes_match_structured', 'ze58_modality_values_and_reports_match_independent_plans'),
    'IDs (extension)': ('ze58_full_u128_ties_remain_ordered', 'ze58_modality_values_and_reports_match_independent_plans'),
    'ze.vector_search': ('ze58_modes_and_components_preserve_provenance', 'ze58_runtime_refusals_return_no_partial_result'),
    'ze.text_search': ('ze58_text_search_returns_real_rows', 'ze58_search_compile_rejections_publish_nothing'),
    'ze.hybrid_search': ('ze58_modes_and_components_preserve_provenance', 'ze58_runtime_refusals_return_no_partial_result'),
}

def check_inventory(log):
    if 'test result: FAILED' in log or 'error:' in log:
        raise ValueError('failed execution log')
    reached = set(re.findall(r'^test (\w+) \.\.\.', log, re.M))
    missing = set(TEST_NAMES) - reached
    if missing:
        raise ValueError(f'unreached inventory tests: {sorted(missing)}')
    summaries = re.findall(r'test result: ok\. (\d+) passed;', log)
    if sum(map(int, summaries)) != len(TEST_NAMES):
        raise ValueError('incomplete terminal test summaries')
    return {name: {'positive': positive, 'boundary': boundary, 'state': 'GREEN'}
            for name, (positive, boundary) in INVENTORY.items()}

def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('checkout', type=Path, nargs='?')
    parser.add_argument('--run', action='store_true')
    parser.add_argument('--log', type=Path)
    parser.add_argument('--write', action='store_true')
    parser.add_argument('--evidence-dir', type=Path, default=ROOT / 'tasks/evidence/ze-59')
    args = parser.parse_args()
    rows = corpus()
    profile = {'format': 'zeppelin-cypher-profile-v1', 'source_commit': PIN,
               'source_verification': 'unverified', 'scenarios': rows,
               'qualification': {'Rust': 'focused', 'C': 'pending', 'Swift': 'pending',
                                 'coverage': 'pending', 'size': 'pending', 'performance': 'pending'}}
    profile['inventory'] = json.loads((FIXTURES / 'binding-manifest.json').read_text())['scenarios']
    if args.checkout:
        inventory, conventions, sources = verify_sources(args.checkout)
        profile.update(source_verification='verified', inventory=inventory['scenarios'],
                       conventions=conventions, originals=sources)
    if args.run:
        args.evidence_dir.mkdir(parents=True, exist_ok=True)
        result = subprocess.run(COMMAND, cwd=ROOT, env={**os.environ, 'CARGO_BUILD_JOBS': '3'},
                                capture_output=True, text=True)
        log = result.stdout + result.stderr
        (args.evidence_dir / 'corpus.log').write_text(log)
        if result.returncode:
            raise ValueError('public execution failed; see corpus.log')
    elif args.log:
        log = args.log.read_text()
    else:
        log = None
    profile['component_history'] = []
    for filename, reason in [('red.log', 'missing public execution receipts'),
                             ('proof-mutants-red.log', 'temporary altered expectations caught by five proof tests')]:
        evidence = ROOT / 'tasks/evidence/ze-59' / filename
        if evidence.exists():
            profile['component_history'].append({'state': 'RED', 'reason': reason,
                                                 'evidence': str(evidence.relative_to(ROOT)),
                                                 'sha256': digest(evidence.read_bytes())})
    if log is not None:
        profile['component_history'].append({'state': 'GREEN', 'reason': 'focused public execution',
                                             'sha256': digest(log)})
        profile['support_evidence'] = check_inventory(log)
        merge_receipts(rows, log, 'CARGO_BUILD_JOBS=3 ' + ' '.join(COMMAND),
                       subprocess.check_output(['git', 'rev-parse', 'HEAD'], cwd=ROOT, text=True).strip(),
                       str(args.log or (args.evidence_dir / 'corpus.log')))
    for row in profile['inventory']:
        base = f'{row["feature"]} [{row["scenario"]}]'
        executions = [r for r in rows if r['coordinate'] == base or r['coordinate'].startswith(base + ' example')]
        row['execution'] = {'state': 'GREEN' if executions and all(r['execution']['state'] == 'GREEN' for r in executions) else 'unexecuted',
                            'coordinates': [r['coordinate'] for r in executions],
                            'original_semantics_verified': profile['source_verification'] == 'verified' and row['support_disposition'] == 'supported'}
    data = json.dumps(profile, indent=2, ensure_ascii=False) + '\n'
    if args.write:
        OUTPUT.write_text(data)
    elif OUTPUT.read_text() != data:
        raise ValueError('profile differs; inspect before --write')
    print(f'{len(rows)} coordinates; {sum(r["execution"]["state"] == "GREEN" for r in rows)} local GREEN; sources {profile["source_verification"]}')
    if args.run and profile['source_verification'] != 'verified':
        raise ValueError('focused execution passed, but pinned source acceptance is unverified')

if __name__ == '__main__':
    main()
