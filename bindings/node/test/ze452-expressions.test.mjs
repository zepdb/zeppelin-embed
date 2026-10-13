import { openGraph } from './graph-fixture.mjs';
import assert from 'node:assert/strict';
import { mkdtempSync, rmSync } from 'node:fs';
import { tmpdir } from 'node:os';
import { join } from 'node:path';
import test from 'node:test';

test('ZE-452 map results retain nested typed values after close', async () => {
  const dir = mkdtempSync(join(tmpdir(), 'ze452-node-'));
  const store = openGraph(join(dir, 'graph'));
  let closed = false;
  try {
    const query = 'RETURN {values:[x IN [1,2,null] WHERE x IS NOT NULL | {n:x}], empty:{}, __proto__:42} AS m';
    const sync = store.cypher(query);
    const async = await store.cypherAsync(query);
    store.close();
    closed = true;
    for (const result of [sync, async]) {
      const map = result.rows[0][0];
      assert.deepEqual(map.values, [{ n: 1n }, { n: 2n }]);
      assert.deepEqual(map.empty, {});
      assert.equal(Object.getPrototypeOf(map), Object.prototype);
      assert.equal(Object.hasOwn(map, '__proto__'), true);
      assert.equal(map.__proto__, 42n);
      assert.deepEqual(result.columns, ['m']);
    }
  } finally { if (!closed) store.close(); rmSync(dir, { recursive: true, force: true }); }
});
