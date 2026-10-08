import { openGraph } from './graph-fixture.mjs';
import assert from 'node:assert/strict';
import { chmodSync, mkdtempSync, rmSync } from 'node:fs';
import { tmpdir } from 'node:os';
import { join } from 'node:path';
import test from 'node:test';
import { Store } from '../index.js';

for (const operation of ['count', 'top-k']) {
  test(`graph ${operation} streams more than 65536 input rows`,
    { skip: !Store.graphSupported() }, () => {
      const directory = mkdtempSync(join(tmpdir(), 'ze-257-query-'));
      let store;
      try {
        store = openGraph(join(directory, 'graph'));
        store.graphApply(Array.from({ length: 17 }, (_, i) => ({
          kind: 'node', operation: 'create', namespace: 'docs', key: String(i),
          revision: 1n, labels: ['Doc'], properties: { rank: BigInt(i) },
        })));
        const match = 'MATCH (a:Doc), (b:Doc), (c:Doc), (d:Doc) ';
        if (operation === 'count') {
          assert.deepEqual(store.cypher(match + 'RETURN count(a)').rows, [[17n ** 4n]]);
        } else {
          assert.deepEqual(store.cypher(match + 'RETURN a.rank AS rank ORDER BY rank DESC LIMIT 5').rows,
            Array.from({ length: 5 }, () => [16n]));
        }
      } finally {
        store?.close();
        rmSync(directory, { recursive: true, force: true });
      }
    });
}

test('graph scan spans more than 64 immutable artifacts', { skip: !Store.graphSupported() }, () => {
  const directory = mkdtempSync(join(tmpdir(), 'ze-257-artifacts-'));
  let store;
  try {
    store = openGraph(join(directory, 'graph'));
    for (let i = 0; i < 80; i++) {
      store.graphApply([{ kind: 'node', operation: 'create', namespace: 'docs', key: String(i),
        revision: 1n, labels: ['Doc'], properties: { rank: BigInt(i) } }]);
    }
    assert.deepEqual(store.cypher('MATCH (n:Doc) RETURN count(n)').rows, [[80n]]);
  } finally {
    store?.close();
    rmSync(directory, { recursive: true, force: true });
  }
});


test('graph calls share the Store close state', {skip: !Store.graphSupported()}, () => {
  const directory = mkdtempSync(join(tmpdir(), 'ze361-close-'));
  const path = join(directory, 'graph');
  const s = openGraph(path);
  try {
    s.graphApply([{kind: 'node', operation: 'create', namespace: 'docs', key: 'durable', revision: 1n}]);
    s.close();
    assert.throws(() => s.cypher('RETURN 1'), {code: 'ZE_ERR_CLOSED'});
    assert.throws(() => s.graphApply([]), {code: 'ZE_ERR_CLOSED'});
    const reopened = openGraph(path);
    try { assert.deepEqual(reopened.cypher('MATCH (n) RETURN count(n)').rows, [[1n]]); } finally { reopened.close(); }
  } finally { rmSync(directory, {recursive: true, force: true}); }
});
