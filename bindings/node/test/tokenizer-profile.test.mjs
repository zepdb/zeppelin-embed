import assert from 'node:assert/strict';
import { mkdtempSync, rmSync } from 'node:fs';
import { tmpdir } from 'node:os';
import { join } from 'node:path';
import { createRequire } from 'node:module';
import test from 'node:test';
const { openNamespace } = createRequire(import.meta.url)('..');

test('namespace tokenizer profiles persist and analyze transcript text', () => {
  const root = mkdtempSync(join(tmpdir(), 'ze-profile-'));
  let store;
  try {
    for (const tokenizerProfile of ['textDefault', 'voice', 'code']) {
      const spec = { tokenizerProfile };
      store = openNamespace(root, tokenizerProfile, spec);
      store.upsert([{ id: 1n, text: 'we recorded twenty five running sessions if needed' }]);
      store.close();
      store = openNamespace(root, tokenizerProfile, spec);
      assert.deepEqual(store.query({ text: tokenizerProfile === 'code' ? 'if' : '25', k: 5 }).hits.map(h => h.id), [1n]);
      if (tokenizerProfile === 'code') assert.deepEqual(store.query({ text: 'run', k: 5 }).hits, []);
      assert.throws(() => store.query({ vector: new Float32Array([1]), k: 5 }), e => e.code === 'ZE_ERR_NO_VECTOR_SPACE');
      store.close(); store = undefined;
      assert.throws(() => { store = openNamespace(root, tokenizerProfile, { tokenizerProfile: tokenizerProfile === 'code' ? 'voice' : 'code' }); }, e => e.code === 'ZE_ERR_EPOCH_MISMATCH');
    }
  } finally {
    store?.close();
    rmSync(root, { recursive: true, force: true });
  }
});

test('invalid namespace tokenizer profile is rejected', () => {
  const root = mkdtempSync(join(tmpdir(), 'ze-profile-invalid-'));
  let store;
  try {
    for (const tokenizerProfile of ['typo', 2, null]) {
      assert.throws(() => { store = openNamespace(root, 'invalid', { tokenizerProfile }); });
    }
  } finally {
    store?.close();
    rmSync(root, { recursive: true, force: true });
  }
});
