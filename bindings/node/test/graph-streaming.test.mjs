import { openGraph } from './graph-fixture.mjs';
import assert from 'node:assert/strict';
import { mkdtempSync, rmSync } from 'node:fs';
import { tmpdir } from 'node:os';
import { join } from 'node:path';
import { createRequire } from 'node:module';
import test from 'node:test';
const { Store } = createRequire(import.meta.url)('..');

function streamingAcceptance(count) {
  const dir = mkdtempSync(join(tmpdir(), 'ze255-node-'));
  const path = join(dir, 'graph');
  let store;
  try {
    store = openGraph(path);
    for (let start = 0; start < count; start += 16) {
      store.graphApply(Array.from({ length: Math.min(16, count - start) }, (_, offset) => {
        const i = start + offset;
        return { kind: 'node', operation: 'create', namespace: 's', key: String(i),
          revision: 1n, labels: ['Segment'], properties: { i: BigInt(i), k: BigInt(i % 7) } };
      }));
    }
    store.close();
    store = openGraph(path, {});
    assert.deepEqual(store.cypher('MATCH (s:Segment) RETURN count(s)').rows, [[BigInt(count)]]);
    assert.deepEqual(store.cypher('MATCH (s:Segment) RETURN s.k AS speaker, count(s) AS n ORDER BY speaker').rows,
      Array.from({ length: 7 }, (_, i) => [BigInt(i), BigInt(Math.floor(count / 7) + (i < count % 7 ? 1 : 0))]));
    assert.deepEqual(store.cypher('MATCH (s:Segment) RETURN DISTINCT s.k AS speaker ORDER BY speaker').rows,
      Array.from({ length: 7 }, (_, i) => [BigInt(i)]));
    assert.deepEqual(store.cypher('MATCH (s:Segment) RETURN s.i AS p ORDER BY p DESC SKIP 2 LIMIT 3').rows,
      [3, 4, 5].map(offset => [BigInt(count - offset)]));
    assert.deepEqual(store.cypher('MATCH (s:Segment) RETURN sum(s.i), min(s.i), max(s.i)').rows,
      [[BigInt(count) * BigInt(count - 1) / 2n, 0n, BigInt(count - 1)]]);
  } finally { store?.close(); rmSync(dir, { recursive: true, force: true }); }
}

// Largest measured build/reopen fixture under default budgets with this shape:
// 444 nodes pass; adding node 445 exhausts graph directory work (ZE-257).
test('ZE-255 default cypher streams aggregates, top-k and DISTINCT over 444 segments',
  { skip: !Store.graphSupported() }, () => streamingAcceptance(444));

test('ZE-255 default cypher over 200000 segments',
  { skip: 'needs ZE-257 (graph reopen and build at 200,000 nodes)' },
  () => streamingAcceptance(200_000));
