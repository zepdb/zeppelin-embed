import assert from 'node:assert/strict';
import { mkdtempSync, rmSync } from 'node:fs';
import { tmpdir } from 'node:os';
import { join } from 'node:path';
import { createRequire } from 'node:module';
import test from 'node:test';
const { openNamespace } = createRequire(import.meta.url)('..');

test('query eligibleIds filters vector, lexical and hybrid before top-k', async () => {
  const root = mkdtempSync(join(tmpdir(), 'ze351-'));
  const store = openNamespace(root, 'eligible', { vectorSpace: { dimensions: 2 } });
  try {
    store.upsert([1n, 2n, 3n].map((id) => ({ id, revision: 1n, timestamp: id,
      vector: new Float32Array([Number(id), 1]), text: 'amber cedar' })));
    store.seal();
    store.upsert([{ id: 4n, revision: 1n, timestamp: 4n,
      vector: new Float32Array([4, 1]), text: 'amber cedar' }]);
    for (const legs of [{ text: 'amber' }, { vector: new Float32Array([1, 1]) },
      { text: 'amber', vector: new Float32Array([1, 1]) }]) {
      const request = { ...legs, k: 2, ...(legs.vector ? { tier: 'exact' } : {}) };
      const all = store.query({ ...request, k: 4 });
      assert.equal(all.hits.length, 4);
      assert.deepEqual(store.query({ ...request, eligibleIds: [] }).hits, []);
      const result = store.query({ ...request, eligibleIds: [4n, 3n, 4n, 999n] });
      assert.equal(result.hits.length, 2);
      assert.deepEqual(new Set(result.hits.map((hit) => hit.id)), new Set([3n, 4n]));
      assert.deepEqual((await store.queryAsync({ ...request, eligibleIds: [4n] })).hits.map((hit) => hit.id), [4n]);
      if (legs.text) {
        const snippets = store.query({ ...request, eligibleIds: [4n], snippetBytes: 32 });
        assert.deepEqual(snippets.hits.map((hit) => hit.id), [4n]);
        assert.ok(snippets.hits[0].snippet);
      }
      assert.deepEqual(store.query({ ...request, eligibleIds: [4n], timestampRange: { start: 0n, end: 4n } }).hits, []);
    }
    for (const eligibleIds of [1n, [1], [-1n], [1n << 128n], ['bad']]) {
      assert.throws(() => store.query({ text: 'amber', eligibleIds }));
    }
  } finally { store.close(); rmSync(root, { recursive: true, force: true }); }
});
