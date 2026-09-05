#!/usr/bin/env python3
"""Frozen full-corpus calibration screens for text-user-bench (not qualification).

Run labels in A/B/C, C/B/A, A/B/C order. Each label is an independently
launched process. Adjacent comparisons therefore share their middle control.
No building, ingesting or reference inference belongs in a timed cell.
"""
import argparse
import hashlib
import json
import math
import os
from pathlib import Path
import platform
import statistics
import subprocess
import sys
import time


def sha(path):
    digest = hashlib.sha256()
    with Path(path).open('rb') as stream:
        for block in iter(lambda: stream.read(1024 * 1024), b''):
            digest.update(block)
    return digest.hexdigest()


def identity(path):
    path = Path(path).resolve()
    files = sorted(p for p in path.rglob('*') if p.is_file()) if path.is_dir() else [path]
    return {'path': str(path), 'files': [
        {'name': str(p.relative_to(path)) if path.is_dir() else p.name,
         'bytes': p.stat().st_size, 'sha256': sha(p)} for p in files]}


def write(path, data):
    Path(path).write_text(json.dumps(data, indent=2, allow_nan=False) + '\n')


def order(count, seed):
    result = list(range(count))
    mask = (1 << 64) - 1
    state = seed
    for upper in range(count - 1, 0, -1):
        state = (state + 0x9e3779b97f4a7c15) & mask
        state = ((state ^ (state >> 30)) * 0xbf58476d1ce4e5b9) & mask
        state = ((state ^ (state >> 27)) * 0x94d049bb133111eb) & mask
        state ^= state >> 31
        other = state % (upper + 1)
        result[upper], result[other] = result[other], result[upper]
    return result


def freeze(source, target, count, seed):
    """Selection uses IDs only, before any ranking or relevance outcome is read."""
    queries = [json.loads(line)['_id'] for line in (source / 'queries.jsonl').read_text().splitlines()]
    lines = (source / 'qrels/test.tsv').read_text().splitlines()
    judged = {line.split('\t')[0] for line in lines[1:] if line}
    eligible = [qid for qid in queries if qid in judged]
    if len(eligible) != len(judged) or not eligible:
        raise ValueError('qrels and query identities do not match')
    selected = {eligible[index] for index in order(len(eligible), seed)[:count]}
    split = {'seed': seed, 'selection': 'Rust shuffled_order v1; first N judged query IDs',
             'calibration_ids': [qid for qid in eligible if qid in selected],
             'held_out_ids': [qid for qid in eligible if qid not in selected]}
    target.mkdir(parents=True)
    (target / 'qrels').mkdir()
    for name in ['corpus.jsonl', 'queries.jsonl']:
        (target / name).symlink_to((source / name).resolve())
    (target / 'qrels/test.tsv').write_text('\n'.join(
        [lines[0]] + [line for line in lines[1:] if line.split('\t')[0] in selected]) + '\n')
    write(target.parent / 'split.json', split)
    return split


def snapshot():
    result = {'time_utc': time.strftime('%Y-%m-%dT%H:%M:%SZ', time.gmtime()),
              'load_average': os.getloadavg()}
    if sys.platform == 'darwin':
        for name, command in [('thermal', ['pmset', '-g', 'therm']),
                              ('power', ['pmset', '-g', 'batt'])]:
            result[name] = subprocess.run(command, capture_output=True, text=True).stdout
    return result


def execute(command, prefix, env):
    before = snapshot()
    with prefix.with_suffix('.stdout').open('wb') as stdout, prefix.with_suffix('.stderr').open('wb') as stderr:
        started = time.monotonic()
        process = subprocess.Popen(command, stdout=stdout, stderr=stderr, env=env)
        _, status, usage = os.wait4(process.pid, 0)
        process.returncode = os.waitstatus_to_exitcode(status)
        receipt = {'argv': command, 'pid': process.pid, 'exit_code': process.returncode,
                   'wall_seconds': time.monotonic() - started,
                   'peak_rss_bytes': usage.ru_maxrss * (1 if sys.platform == 'darwin' else 1024),
                   'cpu_user_seconds': usage.ru_utime, 'cpu_system_seconds': usage.ru_stime,
                   'before': before, 'after': snapshot()}
    write(prefix.with_suffix('.process.json'), receipt)
    if process.returncode:
        raise RuntimeError(f'{prefix.name} exited {process.returncode}; see its stderr')
    return receipt


def metrics(result, qrels):
    parent_scores, chunk_scores, counts = [], [], []
    for ranking in result['rankings']:
        grades = qrels[ranking['query_id']]
        ideal = sum(g / math.log2(i + 2) for i, g in enumerate(sorted(grades.values(), reverse=True)[:result['k']]))
        ids = ranking['doc_ids']
        unique = list(dict.fromkeys(ids))
        counts.append(len(unique))
        if ideal:
            parent_scores.append(sum(grades.get(qid, 0) / math.log2(i + 2) for i, qid in enumerate(unique[:result['k']])) / ideal)
            seen = set()
            dcg = 0.0
            for i, qid in enumerate(ids[:result['k']]):
                if qid not in seen:
                    dcg += grades.get(qid, 0) / math.log2(i + 2)
                seen.add(qid)
            chunk_scores.append(dcg / ideal)
    return {'unique_parent_ndcg_at_k': statistics.mean(parent_scores),
            'parent_gain_at_original_chunk_ranks_ndcg_at_k': statistics.mean(chunk_scores),
            'mean_unique_parents': statistics.mean(counts), 'min_unique_parents': min(counts),
            'chunk_metric_policy': 'first parent gains once at original chunk rank; parent-qrels ideal DCG'}


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--binary', action='append', required=True, help='label=/absolute/preserved/binary')
    parser.add_argument('--beir-root', type=Path, required=True)
    parser.add_argument('--corpus', default='fiqa')
    parser.add_argument('--store', type=Path, required=True)
    parser.add_argument('--bundle', type=Path, required=True)
    parser.add_argument('--coreml', type=Path)
    parser.add_argument('--coreml-tokens', type=int, default=64)
    parser.add_argument('--expect', choices=['scan', 'graph'], required=True)
    parser.add_argument('--out', type=Path, required=True, help='fresh output directory')
    parser.add_argument('--source-record', type=Path, required=True, help='JSON mapping labels to commit/build flags/diff hashes')
    parser.add_argument('--queries', type=int, default=64)
    parser.add_argument('--rounds', type=int, default=1)
    parser.add_argument('--warm', type=int, default=20)
    parser.add_argument('--k', type=int, default=10)
    parser.add_argument('--seed', type=lambda value: int(value, 0), default=0x5eed)
    parser.add_argument('--legs', nargs='+', default=['dense', 'lexical', 'hybrid'])
    parser.add_argument('--allow-work-changes', action='store_true', help='declared work-changing treatment; still record parity')
    parser.add_argument('--allow-ranking-changes', action='store_true', help='declared ranking treatment; still record quality and parity')
    args = parser.parse_args()
    if not 1 <= args.queries <= 64 or args.rounds < 1:
        parser.error('screen requires 1..64 queries and positive rounds')
    binaries = dict(item.split('=', 1) for item in args.binary)
    if len(binaries) != len(args.binary) or len(binaries) < 2:
        parser.error('provide at least two distinct binary labels')
    args.out.mkdir(parents=True, exist_ok=False)
    split = freeze(args.beir_root / args.corpus, args.out / 'calibration' / args.corpus, args.queries, args.seed)
    source_record = json.loads(args.source_record.read_text())
    if set(source_record) != set(binaries):
        raise ValueError('source-record labels must exactly match binary labels')
    env = os.environ.copy()
    env.pop('ZE_QUERY_COREML', None)
    env.pop('ZE_QUERY_COREML_TOKENS', None)
    if args.coreml:
        env['ZE_QUERY_COREML'] = str(args.coreml.resolve())
        env['ZE_QUERY_COREML_TOKENS'] = str(args.coreml_tokens)
    provenance = {'schema_version': 1, 'qualification': 'provisional calibration screen',
                  'driver_sha256': sha(__file__),
                  'hardware': platform.platform(), 'machine': platform.machine(),
                  'cpu_count': os.cpu_count(), 'source': source_record,
                  'binaries': {name: identity(path) for name, path in binaries.items()},
                  'bundle': identity(args.bundle), 'sidecar': identity(args.coreml) if args.coreml else None,
                  'store_before': identity(args.store),
                  'id_map': identity(args.store.parent / f'{args.corpus}-ids.tsv'),
                  'inputs': {name: identity(args.beir_root / args.corpus / name)
                             for name in ['corpus.jsonl', 'queries.jsonl', 'qrels/test.tsv']},
                  'split': split, 'split_sha256': sha(args.out / 'calibration/split.json'),
                  'argv': sys.argv, 'query_environment': {key: value for key, value in env.items()
                                                        if key.startswith('ZE_')},
                  'interference_policy': 'operator must keep task builds/ingest/inference stopped',
                  'graph_status': 'measured only when --expect graph verification succeeds'}
    if sys.platform == 'darwin':
        provenance['hardware_detail'] = subprocess.run(['system_profiler', 'SPHardwareDataType'], capture_output=True, text=True).stdout
    write(args.out / 'provenance.json', provenance)
    for name, binary in binaries.items():
        execute([binary, 'verify', '--bundle', str(args.bundle), '--store', str(args.store), '--expect', args.expect], args.out / f'verify-{name}', env)
    qrels = {}
    for line in (args.out / 'calibration' / args.corpus / 'qrels/test.tsv').read_text().splitlines()[1:]:
        query, document, grade = line.split('\t')
        qrels.setdefault(query, {})[document] = int(grade)
    results, baseline_rankings, baseline_chunks, baseline_work = [], {}, {}, {}
    labels = list(binaries)
    for repetition in range(3):
        for leg in args.legs:
            for name in labels if repetition != 1 else reversed(labels):
                prefix = args.out / f'rep{repetition + 1}-{leg}-{name}'
                output = str(prefix) + '.json'
                command = [binaries[name], 'steady', '--beir-root', str(args.out / 'calibration'),
                           '--corpus', args.corpus, '--bundle', str(args.bundle), '--store', str(args.store),
                           '--legs', leg, '--tier', 'unset', '--rounds', str(args.rounds), '--warm', str(args.warm),
                           '--k', str(args.k), '--seed', hex(args.seed), '--out', output]
                print(f'RUN {prefix.name}', flush=True)
                receipt = execute(command, prefix, env)
                result = json.loads(Path(output).read_text())
                if result['queries'] != len(split['calibration_ids']) or result['samples'] != result['queries'] * args.rounds:
                    raise ValueError('query/sample count mismatch')
                ranking = result['rankings']
                same_rankings = ranking == baseline_rankings.setdefault(leg, ranking)
                chunks = [sample['chunks'] for sample in result.get('query_samples', [])]
                same_chunks = chunks == baseline_chunks.setdefault(leg, chunks) if chunks else None
                work = [sample['diagnostics']['counters'] for sample in result.get('query_samples', [])]
                same_work = work == baseline_work.setdefault(leg, work) if work else None
                row = {'repetition': repetition + 1, 'leg': leg, 'label': name,
                       'summary_ms': result['summary'], 'peak_rss_bytes': receipt['peak_rss_bytes'],
                       'metrics': metrics(result, qrels), 'identical_parent_order': same_rankings,
                       'identical_chunk_identity_score_bits': same_chunks,
                       'identical_work_counters': same_work,
                       'mean_work_counters': {key: statistics.mean(sample[key] for sample in work)
                                              for key in work[0]} if work else None,
                       'reported_backend': result['backend'], 'raw': output}
                results.append(row)
                write(args.out / 'summary.json', results)
                print(json.dumps(row), flush=True)
                ranking_changed = not same_rankings or same_chunks is False
                if (ranking_changed and not args.allow_ranking_changes) or (same_work is False and not args.allow_work_changes):
                    raise ValueError('ranking, score or work changed; screen stopped for investigation')
    write(args.out / 'store-after.json', identity(args.store))
    print('COMPLETE: provisional screen; all three repetitions retained', flush=True)


if __name__ == '__main__':
    main()
