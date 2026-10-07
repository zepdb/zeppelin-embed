import assert from 'node:assert/strict';
import { mkdtempSync, rmSync, readFileSync, writeFileSync, appendFileSync, readdirSync } from 'node:fs';
import { tmpdir } from 'node:os';
import { join } from 'node:path';
import { createRequire } from 'node:module';
import test from 'node:test';
const { openNamespace } = createRequire(import.meta.url)('..');
const spec = { attributes: [] };

test('open reports schema migration once and auto-seal WAL rotation', () => {
  const root = mkdtempSync(join(tmpdir(), 'ze-migrations-'));
  let store;
  try {
    store = openNamespace(root, 'notes', spec);
    assert.deepEqual(store.migrations, []);
    store.upsert([{ id: 1n, text: 'meeting notes' }]);
    store.close(); store = undefined;
    const evolved = { attributes: [{ id: 1, name: 'lang', type: 'dictionaryString', nullable: true }] };
    store = openNamespace(root, 'notes', evolved, { autoSealRows: 10 });
    assert.deepEqual(store.migrations.map(({kind, fromFormat, toFormat}) => ({kind, fromFormat, toFormat})), [
      {kind: 'schema-added', fromFormat: 'manifest/2', toFormat: 'manifest/2'},
      {kind: 'wal-rotated', fromFormat: 'wal/1', toFormat: 'wal/1'},
    ]);
    assert.equal(typeof store.migrations[0].generation, 'bigint');
    store.close(); store = undefined;
    store = openNamespace(root, 'notes', evolved);
    assert.deepEqual(store.migrations, []);
  } finally { store?.close(); rmSync(root, {recursive: true, force: true}); }
});

test('open reports an interrupted final WAL record cut', () => {
  const root = mkdtempSync(join(tmpdir(), 'ze-tail-cut-'));
  let store;
  try {
    store = openNamespace(root, 'notes', spec);
    store.upsert([{id: 1n, text: 'kept'}]);
    store.close(); store = undefined;
    appendFileSync(join(root, 'notes', 'wal.ze'), Buffer.from([1, 2]));
    store = openNamespace(root, 'notes', spec);
    assert.deepEqual(store.migrations.map(m => m.kind), ['wal-tail-cut']);
    assert.equal(store.count().count, 1n);
  } finally { store?.close(); rmSync(root, {recursive: true, force: true}); }
});

test('old and future manifest formats fail with distinct codes and preserve bytes', () => {
  for (const [version, code] of [[1, 'ZE_ERR_FORMAT_VERSION'], [4, 'ZE_ERR_FORMAT_TOO_NEW']]) {
    const root = mkdtempSync(join(tmpdir(), 'ze-format-'));
    try {
      openNamespace(root, 'notes', spec).close();
      const file = join(root, 'notes', 'manifest.ze');
      const bytes = readFileSync(file); bytes.writeUInt16LE(version, 10); writeFileSync(file, bytes);
      assert.throws(() => openNamespace(root, 'notes', spec), error => error.code === code);
      assert.deepEqual(readFileSync(file), bytes);
    } finally { rmSync(root, {recursive: true, force: true}); }
  }
});

test('reindexText preserves text-only segments, active writes, revisions and lexical search on reopen', () => {
  const root = mkdtempSync(join(tmpdir(), 'ze-reindex-'));
  let store;
  try {
    store = openNamespace(root, 'notes', spec);
    for (let id = 1n; id <= 3n; id++) {
      store.upsert([{id, revision: 7n, text: `meeting notes ${id}`}]);
      if (id < 3n) store.seal();
    }
    const before = store.get([1n, 2n, 3n], {text: true});
    const generation = store.reindexText().generation;
    assert.equal(typeof generation, 'bigint');
    assert.deepEqual(store.get([1n, 2n, 3n], {text: true}).documents, before.documents);
    assert.equal(store.query({text: 'meeting', k: 10}).hits.length, 3);
    store.close(); store = undefined;
    store = openNamespace(root, 'notes', spec, {readOnly: true});
    assert.equal(store.query({text: 'meeting', k: 10}).hits.length, 3);
    assert.throws(() => store.reindexText(), error => error.code === 'ZE_ERR_ACCESS_MODE');
  } finally { store?.close(); rmSync(root, {recursive: true, force: true}); }
});

test('autoMerge on open preserves merge behavior without reporting a WAL rotation for sealed data', () => {
  const root = mkdtempSync(join(tmpdir(), 'ze-migration-merge-'));
  let store;
  try {
    store = openNamespace(root, 'notes', spec);
    for (let id = 1n; id <= 3n; id++) {
      store.upsert([{id, text: 'sealed meeting'}]);
      store.seal();
    }
    store.close(); store = undefined;
    store = openNamespace(root, 'notes', spec, {autoMerge: true});
    assert.deepEqual(store.migrations, []);
    assert.equal(store.count().count, 3n);
    assert.equal(readdirSync(join(root, 'notes')).filter(name => name.endsWith('.zseg')).length, 1);
    assert.equal(store.merge().generation, store.seal().generation);
  } finally { store?.close(); rmSync(root, {recursive: true, force: true}); }
});
