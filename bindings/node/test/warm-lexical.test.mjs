import test from 'node:test';
import assert from 'node:assert/strict';
import { mkdtempSync, rmSync } from 'node:fs';
import { tmpdir } from 'node:os';
import { join } from 'node:path';
import { createRequire } from 'node:module';
const { openNamespace } = createRequire(import.meta.url)('..');

test('ZE265 reopen and warm preserves exact multi-term and prefix IDs and scores', async () => {
  const root = mkdtempSync(join(tmpdir(), 'ze265-'));
  let store = openNamespace(root, 'records', {});
  try {
    store.upsert([{ id: 1n, text: 'common pair' }, { id: 2n, text: 'other paint' }]);
    store.seal();
    store.merge();
    const exact = { text: 'common pair', k: 10 };
    const prefix = { text: 'common pai', lastAsPrefix: true, k: 10 };
    const expected = [store.query(exact).hits, store.query(prefix).hits];
    assert.equal(expected[0].length, 1);
    assert.equal(expected[1].length, 2);
    store.close();
    store = openNamespace(root, 'records', {});
    assert.equal(store.warmLexical(), undefined);
    assert.equal(await store.warmLexicalAsync(), undefined);
    assert.deepEqual(store.query(exact).hits, expected[0]);
    assert.deepEqual((await store.queryAsync(prefix)).hits, expected[1]);
    store.upsert([{ id: 3n, text: 'common pair' }]);
    await store.warmLexicalAsync();
    assert.equal(store.query(exact).hits.length, 2);
  } finally { store.close(); rmSync(root, { recursive: true, force: true }); }
});

test('ZE265 async warm cancellation, close race and closed errors', async () => {
  const root = mkdtempSync(join(tmpdir(), 'ze265-async-'));
  const store = openNamespace(root, 'records', {});
  let closed = false;
  try {
    const controller = new AbortController();
    controller.abort();
    await assert.rejects(store.warmLexicalAsync({ signal: controller.signal }), { code: 'ZE_ERR_CANCELLED' });
    const pending = store.warmLexicalAsync();
    // Closing while work is queued must settle with success or a typed error.
    const settled = pending.then(value => ({ value }), error => ({ error }));
    store.close();
    closed = true;
    const outcome = await settled;
    if (outcome.error) {
      assert.ok(['ZE_ERR_CLOSED', 'ZE_ERR_CLOSING', 'ZE_ERR_CANCELLED'].includes(outcome.error.code));
    } else assert.equal(outcome.value, undefined);
    assert.throws(() => store.warmLexical(), { code: 'ZE_ERR_CLOSED' });
    await assert.rejects(store.warmLexicalAsync(), { code: 'ZE_ERR_CLOSED' });
  } finally { if (!closed) store.close(); rmSync(root, { recursive: true, force: true }); }
});


test('ZE265 warming accepts deadlines and rejects conflicting controls', async () => {
  const root = mkdtempSync(join(tmpdir(), 'ze265-deadline-'));
  const store = openNamespace(root, 'records', {});
  try {
    assert.throws(() => store.warmLexical({ deadlineNs: 1n }), { code: 'ZE_ERR_TIMEOUT' });
    await assert.rejects(store.warmLexicalAsync({ deadlineNs: 1n }), { code: 'ZE_ERR_TIMEOUT' });
    const controller = new AbortController();
    await assert.rejects(store.warmLexicalAsync({ signal: controller.signal, deadlineNs: 1000000000n }),
      { code: 'ZE_ERR_INVALID_ARGUMENT' });
    assert.equal(store.warmLexical({ deadlineNs: 1000000000n }), undefined);
    assert.equal(await store.warmLexicalAsync({ deadlineNs: 1000000000n }), undefined);
  } finally { store.close(); rmSync(root, { recursive: true, force: true }); }
});
