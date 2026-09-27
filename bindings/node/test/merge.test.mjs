import assert from 'node:assert/strict';
import { mkdtempSync, readdirSync, rmSync } from 'node:fs';
import { tmpdir } from 'node:os';
import { join } from 'node:path';
import { performance } from 'node:perf_hooks';
import { createRequire } from 'node:module';
import test from 'node:test';

const { openNamespace } = createRequire(import.meta.url)('..');
const options = { durability: 'durable', commitTier: 'none', autoSealRows: 1 };
const spec = { attributes: [{ id: 1, name: 'group', type: 'dictionaryString' }] };

test('idle merge keeps the first reopened query flat across 1000 seals', (t) => {
  const root = mkdtempSync(join(tmpdir(), 'zeppelin-merge-'));
  let store;
  const timings = [];
  try {
    store = openNamespace(root, 'notes', spec, options);
    for (let id = 1; id <= 1000; id++) {
      store.upsert([{ id: BigInt(id), text: `shared harbour document ${id}`, metadata: new Uint8Array([id % 256]), attributes: [{ id: 1, type: 'string', value: 'notes' }] }]);
      if (id % 8 === 0) {
        store.seal();
        assert.equal(typeof store.merge().generation, 'bigint');
      }
      if (id === 100 || id === 1000) {
        store.seal();
        store.merge();
        store.close();
        store = openNamespace(root, 'notes', spec, options);
        const start = performance.now();
        const hits = store.query({ text: 'harbour', k: 10 }).hits;
        timings.push(performance.now() - start);
        assert.equal(hits.length, 10);
        assert.equal(store.count().count, BigInt(id));
        assert.equal(readdirSync(join(root, 'notes')).filter(f => f.endsWith('.zseg')).length, 1);
        assert.equal(store.get([BigInt(id)], { text: true }).documents[0].text, `shared harbour document ${id}`);
      }
    }
    t.diagnostic(`first query after 100/1000 seals (ms): ${timings.join(', ')}`);
    // Supporting wall-clock evidence, with a broad floor for loaded hosts.
    assert.ok(timings[1] < Math.max(50, timings[0] * 4), `first-query times ${timings}`);
    const generation = store.merge().generation;
    assert.equal(store.merge().generation, generation);
  } finally {
    store?.close();
    rmSync(root, { recursive: true, force: true });
  }
});
