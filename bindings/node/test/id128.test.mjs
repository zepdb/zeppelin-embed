import assert from 'node:assert/strict';
import { mkdtempSync, rmSync } from 'node:fs';
import { tmpdir } from 'node:os';
import { join } from 'node:path';
import { createRequire } from 'node:module';
import test from 'node:test';
const { openNamespace, uuidToId, idToUuid } = createRequire(import.meta.url)('..');
const spec = { attributes: [{ id: 1, name: 'parent', type: 'id128', nullable: true }] };
const attr = value => ({ id: 1, type: 'id128', value });
const filter = (op, values) => ({ op, attributeId: 1, values: values.map(attr) });

test('id128 attributes survive WAL, seal, compaction and reopen', () => {
  const root = mkdtempSync(join(tmpdir(), 'ze-id128-'));
  const uuid = 'fedcba98-7654-3210-fedc-ba9876543210';
  const values = [0n, 1n, (1n << 127n) + 1n, (1n << 128n) - 1n, uuidToId(uuid)];
  let store;
  const check = () => {
    const documents = store.get([1n, 2n, 3n, 4n, 5n], { attributes: true }).documents;
    assert.deepEqual(documents.map(d => d.attributes[0].value), values);
    assert.equal(idToUuid(documents[4].attributes[0].value), uuid);
    assert.deepEqual(store.scan({ filter: filter('eq', [values[2]]) }).documents.map(d => d.id), [3n]);
    assert.deepEqual(store.scan({ filter: filter('in', [values[1], values[3]]) }).documents.map(d => d.id), [2n, 4n]);
    assert.equal(store.count({ filter: { op: 'isNull', attributeId: 1 } }).count, 1n);
  };
  try {
    store = openNamespace(root, 'ids', spec);
    store.upsert(values.map((value, i) => ({ id: BigInt(i + 1), attributes: [attr(value)] })));
    store.upsert([{ id: 6n }]);
    check();
    store.close();
    store = openNamespace(root, 'ids', spec);
    check();
    store.upsert([{ id: 8n, attributes: [attr(42n)] }]);
    store.seal();
    store.upsert([{ id: 7n, attributes: [attr(42n)] }]);
    check();
    // Rewrite the sealed segment while retaining all original ID attributes.
    store.seal();
    assert.equal(store.deleteWhere(filter('eq', [42n])).deleted, 2n);
    assert.equal(store.count().count, 6n);
    check();
    store.close();
    store = openNamespace(root, 'ids', spec);
    check();
  } finally {
    store?.close();
    rmSync(root, { recursive: true, force: true });
  }
});

test('id128 rejects out-of-range values', () => {
  const root = mkdtempSync(join(tmpdir(), 'ze-id128-invalid-'));
  let store;
  try {
    store = openNamespace(root, 'ids', spec);
    for (const value of [-1n, 1n << 128n, 1, 'uuid']) {
      assert.throws(() => store.upsert([{ id: 1n, attributes: [attr(value)] }]));
    }
    assert.equal(store.count().count, 0n);
  } finally {
    store?.close();
    rmSync(root, { recursive: true, force: true });
  }
});
