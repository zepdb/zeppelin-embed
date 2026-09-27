import assert from 'node:assert/strict';
import { chmodSync, mkdtempSync, rmSync } from 'node:fs';
import { tmpdir } from 'node:os';
import { join } from 'node:path';
import test from 'node:test';
import { GraphStore } from '../index.js';

for (const operation of ['count', 'top-k']) {
  test(`graph ${operation} streams more than 65536 input rows`,
    { skip: !GraphStore.isSupported() }, () => {
      const directory = mkdtempSync(join(tmpdir(), 'ze-257-query-'));
      let store;
      try {
        store = GraphStore.open(join(directory, 'graph'));
        store.apply(Array.from({ length: 17 }, (_, i) => ({
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

test('graph scan spans more than 64 immutable artifacts', { skip: !GraphStore.isSupported() }, () => {
  const directory = mkdtempSync(join(tmpdir(), 'ze-257-artifacts-'));
  let store;
  try {
    store = GraphStore.open(join(directory, 'graph'));
    for (let i = 0; i < 80; i++) {
      store.apply([{ kind: 'node', operation: 'create', namespace: 'docs', key: String(i),
        revision: 1n, labels: ['Doc'], properties: { rank: BigInt(i) } }]);
    }
    assert.deepEqual(store.cypher('MATCH (n:Doc) RETURN count(n)').rows, [[80n]]);
  } finally {
    store?.close();
    rmSync(directory, { recursive: true, force: true });
  }
});


test('graph close reports checkpoint failure and remains idempotent',
  { skip: !GraphStore.isSupported() || process.platform !== 'darwin' }, () => {
    const directory = mkdtempSync(join(tmpdir(), 'ze-257-close-'));
    const path = join(directory, 'graph');
    const store = GraphStore.open(path);
    let needsClose = true;
    try {
      store.apply([{ kind: 'node', operation: 'create', namespace: 'docs', key: 'durable',
        revision: 1n, labels: ['Doc'] }]);
      chmodSync(path, 0o500);
      assert.throws(() => store.close(), { code: 'ZE_ERR_IO' });
      needsClose = false;
      chmodSync(path, 0o700);
      assert.doesNotThrow(() => store.close());
      assert.throws(() => store.cypher('RETURN 1'), { code: 'ZE_ERR_CLOSED' });
      const reopened = GraphStore.open(path, { mode: 'readWrite' });
      try { assert.deepEqual(reopened.cypher('MATCH (n:Doc) RETURN count(n)').rows, [[1n]]); }
      finally { reopened.close(); }
    } finally {
      chmodSync(path, 0o700);
      if (needsClose) store.close();
      rmSync(directory, { recursive: true, force: true });
    }
  });
