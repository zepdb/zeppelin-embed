#!/usr/bin/env python3
"""Report per-crate source line coverage without changing exclusions or thresholds."""
import argparse
import json
from collections import defaultdict
from pathlib import Path

parser = argparse.ArgumentParser()
parser.add_argument('report', type=Path)
parser.add_argument('--require', action='append', default=[])
parser.add_argument('--source-root', type=Path, default=Path(__file__).resolve().parents[3])
args = parser.parse_args()
repo = args.source_root.resolve()
groups = defaultdict(lambda: {'covered': 0, 'count': 0, 'files': 0})
seen = set()
for data in json.loads(args.report.read_text())['data']:
    for item in data['files']:
        path = Path(item['filename']).resolve()
        if path in seen:
            raise SystemExit(f'duplicate source file in report: {path}')
        seen.add(path)
        relative = path.relative_to(repo)
        if relative.parts[0] == 'crates':
            package = relative.parts[1]
        elif relative.parts[:2] == ('tests', 'adversarial-oracle'):
            package = 'zeppelin-embed-adversarial-oracle'
        elif relative.parts[0] == 'tests':
            package = 'zeppelin-embed-workspace-tests'
        else:
            package = 'other-repository-source'
        lines = item['summary']['lines']
        group = groups[package]
        group['covered'] += lines['covered']
        group['count'] += lines['count']
        group['files'] += 1
for group in groups.values():
    group['percent'] = 100 * group['covered'] / group['count'] if group['count'] else None
    group['passes_90'] = group['count'] > 0 and group['covered'] * 10 >= group['count'] * 9
print(json.dumps(dict(sorted(groups.items())), indent=2))
failed = [name for name in args.require if name not in groups or not groups[name]['passes_90']]
if failed:
    raise SystemExit('required crates below 90% or absent: ' + ', '.join(failed))
