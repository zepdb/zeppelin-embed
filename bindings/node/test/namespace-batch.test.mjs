import assert from 'node:assert/strict';
import { mkdtempSync, readdirSync, readFileSync, rmSync } from 'node:fs';
import { tmpdir } from 'node:os';
import { join } from 'node:path';
import { createRequire } from 'node:module';
import test from 'node:test';
const { namespaceBatch, openNamespace } = createRequire(import.meta.url)('..');
const spec = { attributes: [{ id: 1, name: 'rank', type: 'u64' }] };
const doc = (id, rank = id) => ({ id, text: `document ${id}`, attributes: [{ id: 1, type: 'u64', value: rank }] });
test('namespaceBatch publishes mixed changes and standalone readers resolve the shared decision', () => {
  const root = mkdtempSync(join(tmpdir(), 'ze-batch-'));
  try {
    for (const name of ['a', 'b']) {
      const store = openNamespace(root, name, spec);
      try { store.upsert([doc(1n), doc(2n), doc(3n), doc(5n)]); store.seal(); store.deleteWhere({ op: 'eq', attributeId: 1, values: [{ id: 1, type: 'u64', value: 5n }] }); } finally { store.close(); }
    }
    const changes = ['a', 'b'].map(name => ({ name, spec, upserts: [doc(4n)], deletes: [1n], deleteWhere: { op: 'eq', attributeId: 1, values: [{ id: 1, type: 'u64', value: 2n }] } }));
    const generations = namespaceBatch(root, changes);
    assert.equal(generations.length, 2); assert.ok(generations.every(g => typeof g === 'bigint' && g > 0n));
    for (const name of ['a', 'b']) {
      const store = openNamespace(root, name, spec, { readOnly: true });
      try {
        const result = store.get([1n, 2n, 3n, 4n]);
        assert.deepEqual(result.documents.map(d => d?.id ?? null), [null, null, 3n, 4n]);
      } finally { store.close(); }
    }
    assert.throws(() => namespaceBatch(root, [
      { name: 'a', spec, upserts: [doc(5n)] },
      { name: 'b', spec, upserts: [{ ...doc(6n), expectedRevision: 99n }] },
    ]));
    const store = openNamespace(root, 'a', spec);
    try { assert.equal(store.get([5n]).documents[0], null); } finally { store.close(); }
  } finally { rmSync(root, { recursive: true, force: true }); }
});


for (const redirected of [false, true]) {
  test(`namespaceBatch preserves snapshot files through idle merge (redirected=${redirected})`, () => {
    const root = mkdtempSync(join(tmpdir(), 'ze-batch-pins-'));
    let writer, view;
    const changes = ['a', 'b'].map(name => ({ name, spec, upserts: [doc(4n)] }));
    const segments = (dir = root) => readdirSync(dir, { withFileTypes: true }).flatMap(entry => {
      const path = join(dir, entry.name);
      return entry.isDirectory() ? segments(path) : entry.name.endsWith('.zseg') ? [path] : [];
    });
    try {
      for (const name of ['a', 'b']) {
        const store = openNamespace(root, name, spec);
        try { store.upsert([doc(1n)]); store.seal(); } finally { store.close(); }
      }
      if (redirected) namespaceBatch(root, changes);
      writer = openNamespace(root, 'a', spec);
      view = writer.openSnapshot();
      const pinned = new Map(segments().map(path => [path, readFileSync(path)]));
      writer.upsert([doc(2n)]); writer.seal(); writer.merge();
      writer.close(); writer = undefined;
      assert.throws(() => namespaceBatch(root, changes), { code: 'ZE_ERR_STORE_BUSY' });
      for (const [path, bytes] of pinned) assert.deepEqual(readFileSync(path), bytes, path);
      assert.equal(view.get([1n]).documents[0].id, 1n);
      assert.equal(view.get([2n]).documents[0], null);
      view.close(); view = undefined;
      namespaceBatch(root, changes);
      const reader = openNamespace(root, 'a', spec, { readOnly: true });
      try { assert.equal(reader.get([4n]).documents[0].id, 4n); } finally { reader.close(); }
    } finally {
      view?.close(); writer?.close(); rmSync(root, { recursive: true, force: true });
    }
  });
}

test('namespaceBatchLive keeps writable handles usable', () => {
  const { namespaceBatchLive } = createRequire(import.meta.url)('..');
  const root = mkdtempSync(join(tmpdir(), 'ze-live-batch-'));
  const liveSpec = {};
  const liveDoc = id => ({ id, text: `document ${id}` });
  const stores = ['a', 'b'].map(name => openNamespace(root, name, liveSpec));
  try {
    stores.forEach(store => store.upsert([liveDoc(1n)]));
    const changes = stores.map((store, i) => ({ store, name: ['a', 'b'][i], spec: liveSpec, upserts: [liveDoc(2n)] }));
    assert.equal(namespaceBatchLive(root, changes).length, 2);
    for (const store of stores) {
      store.upsert([liveDoc(3n)]);
      assert.deepEqual(store.get([1n, 2n, 3n]).documents.map(d => d.id), [1n, 2n, 3n]);
    }
    stores.pop().close();
    assert.throws(() => namespaceBatchLive(root, changes), { code: 'ZE_ERR_CLOSED' });
  } finally { stores.forEach(store => store.close()); rmSync(root, { recursive: true, force: true }); }
});
