#!/usr/bin/env python3
"""Gate the exact unchanged text source inventory; no exclusions or test lines."""
import json
import sys


def inventory(path):
    files = json.load(open(path))['data'][0]['files']
    rows = {}
    for file in files:
        marker = '/crates/zeppelin-embed-text/src/'
        if marker in file['filename']:
            name = file['filename'].split(marker)[1]
            assert name not in rows
            rows[name] = file['summary']['lines']
    return rows


baseline, current = map(inventory, sys.argv[1:])
assert {k: v['count'] for k, v in baseline.items()} == {k: v['count'] for k, v in current.items()}, 'changed source inventory'
assert len(current) == 12
count = sum(v['count'] for v in current.values())
covered = sum(v['covered'] for v in current.values())
assert count == 3550
for name, row in sorted(current.items()):
    print(f"{name}: {row['covered']}/{row['count']} ({row['percent']:.8f}%)")
print(f'EXACT TEXT INVENTORY: {covered}/{count} = {100 * covered / count:.8f}%')
if covered * 100 < count * 90:
    print('FAIL: line coverage below90%')
    sys.exit(1)
print('PASS: line coverage >=90%, unchanged3550-line inventory')
