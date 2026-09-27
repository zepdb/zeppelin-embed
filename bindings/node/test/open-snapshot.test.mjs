import assert from 'node:assert/strict';
import { mkdtempSync, readdirSync, rmSync } from 'node:fs';
import { tmpdir } from 'node:os';
import { join } from 'node:path';
import { createRequire } from 'node:module';
import test from 'node:test';
const { openNamespace } = createRequire(import.meta.url)('..');
const spec = { attributes: [] };
const options = { durability: 'durable', commitTier: 'none' };

test('open_snapshot_is_stable_and_read_only', () => {
  const root = mkdtempSync(join(tmpdir(), 'ze-open-snapshot-'));
  let store, snapshot;
  try {
    store = openNamespace(root, 'notes', spec, options);
    store.upsert([{ id: 1n, revision: 1n, text: 'original sealed' }]);
    store.seal();
    store.upsert([{ id: 2n, revision: 1n, text: 'original active' }]);
    const before = readdirSync(join(root, 'notes')).sort();
    snapshot = store.openSnapshot();
    assert.throws(() => snapshot.openSnapshot(), { code: 'ZE_ERR_ACCESS_MODE' });
    assert.deepEqual(readdirSync(join(root, 'notes')).sort(), before);
    const first = snapshot.scan({ limit: 1, fields: { text: true } });
    store.upsert([{ id: 2n, revision: 2n, text: 'changed active' }, { id: 3n, text: 'new' }]);
    store.delete([1n]);
    store.seal();
    const second = snapshot.scan({ limit: 1, fields: { text: true }, cursor: first.cursor });
    assert.equal(second.generation, first.generation);
    assert.deepEqual([...first.documents, ...second.documents].map(d => d.id).sort(), [1n, 2n]);
    assert.deepEqual(snapshot.get([1n, 2n], { text: true }).documents.map(d => d.text),
      ['original sealed', 'original active']);
    assert.equal(snapshot.count().count, 2n);
    const query = snapshot.query({ text: 'original', k: 10 });
    assert.equal(query.generation, first.generation);
    assert.deepEqual(query.hits.map(hit => hit.id).sort(), [1n, 2n]);
    assert.throws(() => snapshot.delete([1n]), { code: 'ZE_ERR_ACCESS_MODE' });
    assert.throws(() => snapshot.seal(), { code: 'ZE_ERR_ACCESS_MODE' });
    for (const name of before.filter(n => n.endsWith('.zseg')))
      assert.ok(readdirSync(join(root, 'notes')).includes(name), `retained ${name}`);
    store.close(); store = undefined;
    assert.equal(snapshot.get([1n]).documents[0].id, 1n);
    assert.throws(() => openNamespace(root, 'notes', spec, options), { code: 'ZE_ERR_STORE_BUSY' });
    snapshot.close();
    assert.throws(() => snapshot.openSnapshot(), { code: 'ZE_ERR_CLOSED' });
    snapshot = undefined;
    store = openNamespace(root, 'notes', spec, options);
    for (const name of before.filter(n => n.endsWith('.zseg')))
      assert.ok(!readdirSync(join(root, 'notes')).includes(name), `reclaimed ${name}`);
    assert.equal(store.get([1n]).documents[0], null);
  } finally {
    snapshot?.close(); store?.close(); rmSync(root, { recursive: true, force: true });
  }
});


test('snapshot_lifetime_and_purge_protection', () => {
  const root = mkdtempSync(join(tmpdir(), 'ze-snapshot-purge-'));
  let store, first, second;
  try {
    store = openNamespace(root, 'notes', spec, options);
    store.upsert([{ id: 1n, text: 'retained' }]);
    store.seal();
    first = store.openSnapshot();
    second = store.openSnapshot();
    const generation = store.count().generation;
    assert.throws(() => store.purge([1n]), { code: 'ZE_ERR_STORE_BUSY' });
    assert.equal(store.count().generation, generation);
    assert.throws(() => store.deleteWhere({ op: 'and', children: [] }), { code: 'ZE_ERR_STORE_BUSY' });
    assert.equal(store.count().generation, generation);
    first.close(); first = undefined;
    assert.throws(() => store.purge([1n]), { code: 'ZE_ERR_STORE_BUSY' });
    assert.equal(second.get([1n], { text: true }).documents[0].text, 'retained');
    second.close(); second = undefined;
    store.purge([1n]);
    assert.equal(store.get([1n]).documents[0], null);
    store.close(); store = undefined;
  } finally {
    second?.close(); first?.close(); store?.close();
    rmSync(root, { recursive: true, force: true });
  }
});

for (const autoMerge of [false, true]) {
  test(`snapshot cursor survives seal, merge and reopen (autoMerge=${autoMerge})`, () => {
    const root = mkdtempSync(join(tmpdir(), 'ze-snapshot-merge-'));
    let store, view, reader;
    const files = () => readdirSync(join(root, 'notes')).filter(name => name.endsWith('.zseg'));
    try {
      store = openNamespace(root, 'notes', spec, { ...options, autoMerge, ...(autoMerge ? { autoSealRows: 1 } : {}) });
      for (const id of [1n, 2n]) {
        store.upsert([{ id, text: `original ${id}` }]);
        store.seal();
      }
      const pinnedFiles = files();
      store.upsert([{ id: 3n, text: 'original active' }]);
      view = store.openSnapshot();
      const first = view.scan({ limit: 1 });
      // With autoMerge enabled, the next write seals and merges row 3.
      if (!autoMerge) store.seal();
      store.upsert([{ id: 4n, text: 'later' }]);
      store.seal();
      if (!autoMerge) store.merge();
      for (const name of pinnedFiles) assert.ok(files().includes(name), `merge unlinked pinned ${name}`);
      store.close(); store = undefined;
      reader = openNamespace(root, 'notes', spec, { readOnly: true });
      assert.equal(reader.count().count, 4n);
      const ids = first.documents.map(row => row.id);
      let cursor = first.cursor;
      while (cursor !== null) {
        const page = view.scan({ limit: 1, cursor });
        assert.equal(page.generation, first.generation);
        ids.push(...page.documents.map(row => row.id));
        cursor = page.cursor;
      }
      assert.deepEqual(ids.sort(), [1n, 2n, 3n]);
      reader.close(); reader = undefined;
      view.close(); view = undefined;
      store = openNamespace(root, 'notes', spec, options);
      assert.equal(store.count().count, 4n);
      assert.equal(files().length, 1, 'merge published one segment; reopen reclaimed retired files');
      for (const name of pinnedFiles) assert.ok(!files().includes(name));
    } finally {
      reader?.close(); view?.close(); store?.close();
      rmSync(root, { recursive: true, force: true });
    }
  });
}
