import assert from 'node:assert/strict';
import { mkdtempSync, rmSync } from 'node:fs';
import { tmpdir } from 'node:os';
import { join } from 'node:path';
import { spawnSync } from 'node:child_process';
import { createRequire } from 'node:module';
import { fileURLToPath } from 'node:url';
import test from 'node:test';

const require = createRequire(import.meta.url);
const { ZeppelinError, listNamespaces, openNamespace } = require('..');

function temporaryRoot(prefix) {
  return mkdtempSync(join(tmpdir(), prefix));
}

const vectorSpec = {
  attributes: [{ id: 1, name: 'rank', type: 'u64', nullable: false }],
  vectorSpace: { dimensions: 2, normalization: 'none' },
};

test('creates, reopens, validates, and lists namespaces', () => {
  const root = temporaryRoot('zeppelin-node-namespace-');
  let store;
  try {
    store = openNamespace(root, 'records', vectorSpec);
    store.close();
    store = openNamespace(root, 'records', vectorSpec);
    store.close();
    store = undefined;

    assert.throws(
      () =>
        openNamespace(root, 'records', {
          ...vectorSpec,
          attributes: [{ id: 1, name: 'rank', type: 'i64', nullable: false }],
        }),
      (error) =>
        error instanceof ZeppelinError &&
        error.code === 'ZE_ERR_SCHEMA_MISMATCH',
    );
    assert.deepEqual(listNamespaces(root), ['records']);
  } finally {
    store?.close();
    rmSync(root, { force: true, recursive: true });
  }
});

test('upserts and gets every attribute and document field type', () => {
  const root = temporaryRoot('zeppelin-node-upsert-get-');
  const spec = {
    attributes: [
      { id: 1, name: 'u64', type: 'u64' },
      { id: 2, name: 'i64', type: 'i64' },
      { id: 3, name: 'f64', type: 'f64' },
      { id: 4, name: 'bool', type: 'bool' },
      { id: 5, name: 'dict', type: 'dictionaryString' },
      { id: 6, name: 'raw', type: 'rawString' },
    ],
    vectorSpace: { dimensions: 2 },
  };
  const store = openNamespace(root, 'all-types', spec);
  try {
    const mutation = store.upsert([
      {
        id: (3n << 64n) + 5n,
        revision: 7n,
        timestamp: -11n,
        vector: new Float32Array([1.25, -2.5]),
        text: 'stored text',
        metadata: new Uint8Array([7, 8, 9]),
        attributes: [
          { id: 1, type: 'u64', value: 42n },
          { id: 2, type: 'i64', value: -17n },
          { id: 3, type: 'f64', value: 3.5 },
          { id: 4, type: 'bool', value: true },
          { id: 5, type: 'string', value: 'alpha' },
          { id: 6, type: 'string', value: 'bravo' },
        ],
      },
    ]);
    assert.equal(typeof mutation.sequence, 'bigint');

    const result = store.get([(3n << 64n) + 5n]);
    assert.equal(result.missingCount, 0);
    assert.equal(typeof result.generation, 'bigint');
    assert.deepEqual(result.documents[0], {
      id: (3n << 64n) + 5n,
      revision: 7n,
      timestamp: -11n,
      vector: new Float32Array([1.25, -2.5]),
      text: 'stored text',
      metadata: new Uint8Array([7, 8, 9]),
      attributes: [
        { id: 1, type: 'u64', value: 42n },
        { id: 2, type: 'i64', value: -17n },
        { id: 3, type: 'f64', value: 3.5 },
        { id: 4, type: 'bool', value: true },
        { id: 5, type: 'string', value: 'alpha' },
        { id: 6, type: 'string', value: 'bravo' },
      ],
    });
  } finally {
    store.close();
    rmSync(root, { force: true, recursive: true });
  }
});

test('get distinguishes present, missing, tombstoned, and superseded revisions', () => {
  const root = temporaryRoot('zeppelin-node-get-states-');
  const store = openNamespace(root, 'states', {
    vectorSpace: { dimensions: 1 },
  });
  try {
    store.upsert([
      { id: 1n, revision: 1n, vector: new Float32Array([1]), text: 'old' },
      { id: 2n, revision: 1n, vector: new Float32Array([2]) },
    ]);
    store.upsert([
      { id: 1n, revision: 2n, vector: new Float32Array([3]), text: 'new' },
    ]);
    store.delete([2n]);

    const result = store.get([1n, 99n, 2n], { vector: true, text: true });
    assert.equal(result.missingCount, 2);
    assert.equal(result.documents[0].revision, 2n);
    assert.equal(result.documents[0].text, 'new');
    assert.deepEqual(result.documents[0].vector, new Float32Array([3]));
    assert.equal(result.documents[1], null);
    assert.equal(result.documents[2], null);
  } finally {
    store.close();
    rmSync(root, { force: true, recursive: true });
  }
});

test('scan preserves storage and timestamp orders and pages every live row once', () => {
  const root = temporaryRoot('zeppelin-node-scan-');
  const store = openNamespace(root, 'scan', {
    vectorSpace: { dimensions: 1 },
  });
  try {
    store.upsert([
      { id: 3n, timestamp: 20n, vector: new Float32Array([3]) },
      { id: 1n, timestamp: 30n, vector: new Float32Array([1]) },
      { id: 2n, timestamp: 10n, vector: new Float32Array([2]) },
    ]);

    assert.deepEqual(
      store.scan({ limit: 10 }).documents.map((document) => document.id),
      [3n, 1n, 2n],
    );
    assert.deepEqual(
      store
        .scan({ limit: 10, order: 'timestampAscending' })
        .documents.map((document) => document.id),
      [2n, 3n, 1n],
    );
    assert.deepEqual(
      store
        .scan({ limit: 10, order: 'timestampDescending' })
        .documents.map((document) => document.id),
      [1n, 3n, 2n],
    );

    const paged = [];
    let cursor;
    do {
      const page = store.scan({ limit: 1, cursor });
      paged.push(...page.documents.map((document) => document.id));
      cursor = page.cursor ?? undefined;
    } while (cursor !== undefined);
    assert.deepEqual(paged, [3n, 1n, 2n]);
  } finally {
    store.close();
    rmSync(root, { force: true, recursive: true });
  }
});

test('scan rejects a stale cursor instead of restarting', () => {
  const root = temporaryRoot('zeppelin-node-stale-scan-');
  const store = openNamespace(root, 'stale', {
    vectorSpace: { dimensions: 1 },
  });
  try {
    store.upsert([
      { id: 1n, vector: new Float32Array([1]) },
      { id: 2n, vector: new Float32Array([2]) },
    ]);
    const first = store.scan({ limit: 1 });
    assert.notEqual(first.cursor, null);
    store.upsert([{ id: 3n, vector: new Float32Array([3]) }]);
    assert.throws(
      () => store.scan({ limit: 1, cursor: first.cursor }),
      (error) =>
        error instanceof ZeppelinError && error.code === 'ZE_ERR_SCAN_STALE',
    );
  } finally {
    store.close();
    rmSync(root, { force: true, recursive: true });
  }
});

test('count with a filter agrees with scanned documents', () => {
  const root = temporaryRoot('zeppelin-node-count-');
  const store = openNamespace(root, 'count', vectorSpec);
  try {
    store.upsert([
      {
        id: 1n,
        vector: new Float32Array([1, 0]),
        attributes: [{ id: 1, type: 'u64', value: 7n }],
      },
      {
        id: 2n,
        vector: new Float32Array([0, 1]),
        attributes: [{ id: 1, type: 'u64', value: 8n }],
      },
      {
        id: 3n,
        vector: new Float32Array([0.5, 0.5]),
        attributes: [{ id: 1, type: 'u64', value: 7n }],
      },
    ]);
    const filter = {
      op: 'eq',
      attributeId: 1,
      values: [{ id: 1, type: 'u64', value: 7n }],
    };
    const scanned = store.scan({ limit: 10, filter });
    const counted = store.count({ filter });
    assert.equal(counted.count, BigInt(scanned.documents.length));
    assert.equal(counted.generation, scanned.generation);
  } finally {
    store.close();
    rmSync(root, { force: true, recursive: true });
  }
});

test('filtered search equals post-filtered exact search', () => {
  const root = temporaryRoot('zeppelin-node-filtered-search-');
  const store = openNamespace(root, 'search', vectorSpec);
  try {
    store.upsert([
      {
        id: 1n,
        vector: new Float32Array([1, 0]),
        attributes: [{ id: 1, type: 'u64', value: 7n }],
      },
      {
        id: 2n,
        vector: new Float32Array([0, 1]),
        attributes: [{ id: 1, type: 'u64', value: 8n }],
      },
      {
        id: 3n,
        vector: new Float32Array([0.75, 0.25]),
        attributes: [{ id: 1, type: 'u64', value: 7n }],
      },
      {
        id: 4n,
        vector: new Float32Array([0.25, 0.75]),
        attributes: [{ id: 1, type: 'u64', value: 8n }],
      },
    ]);
    const query = new Float32Array([0.9, 0.1]);
    const expected = store
      .searchFiltered(query, { op: 'and', children: [] }, { k: 4, tier: 'exact' })
      .filter((hit) => hit.id % 2n === 1n);
    const actual = store.searchFiltered(
      query,
      {
        op: 'eq',
        attributeId: 1,
        values: [{ id: 1, type: 'u64', value: 7n }],
      },
      { k: 4, tier: 'exact' },
    );
    assert.deepEqual(actual, expected);
  } finally {
    store.close();
    rmSync(root, { force: true, recursive: true });
  }
});

test('record-only namespaces upsert, get, and scan without vector search', () => {
  const root = temporaryRoot('zeppelin-node-record-only-');
  const store = openNamespace(root, 'records', { attributes: [] });
  try {
    store.upsert([{ id: 1n, text: 'record', metadata: new Uint8Array([4]) }]);
    const got = store.get([1n]);
    assert.equal(got.documents[0].text, 'record');
    assert.deepEqual(got.documents[0].metadata, new Uint8Array([4]));
    assert.equal('vector' in got.documents[0], false);
    const scanned = store.scan({ limit: 10 });
    assert.equal(scanned.documents[0].id, 1n);
    assert.equal('vector' in scanned.documents[0], false);
    assert.throws(
      () =>
        store.searchFiltered(
          new Float32Array([1]),
          { op: 'and', children: [] },
          { k: 1 },
        ),
      (error) =>
        error instanceof ZeppelinError &&
        error.code === 'ZE_ERR_NO_VECTOR_SPACE',
    );
  } finally {
    store.close();
    rmSync(root, { force: true, recursive: true });
  }
});

test('malformed filters throw typed errors without hanging', () => {
  const root = temporaryRoot('zeppelin-node-malformed-filter-');
  const store = openNamespace(root, 'malformed', vectorSpec);
  try {
    store.upsert([
      {
        id: 1n,
        vector: new Float32Array([1, 0]),
        attributes: [{ id: 1, type: 'u64', value: 7n }],
      },
    ]);
    const typedInvalid = (error) =>
      error instanceof ZeppelinError && error.code === 'ZE_ERR_INVALID_ARGUMENT';

    assert.throws(
      () => store.count({ filter: { op: 'and', children: new Array(1) } }),
      typedInvalid,
    );

    const cyclic = { op: 'and', children: [] };
    cyclic.children.push(cyclic);
    assert.throws(() => store.count({ filter: cyclic }), typedInvalid);

    let tooDeep = { op: 'exists', attributeId: 1 };
    for (let depth = 0; depth < 34; depth += 1) {
      tooDeep = { op: 'not', children: [tooDeep] };
    }
    assert.throws(() => store.count({ filter: tooDeep }), typedInvalid);
  } finally {
    store.close();
    rmSync(root, { force: true, recursive: true });
  }
});

test('repeated get and scan result allocation reaches a memory plateau', (t) => {
  const packageDirectory = fileURLToPath(new URL('..', import.meta.url));
  const script = String.raw`
    const assert = require('node:assert/strict');
    const { mkdtempSync, rmSync } = require('node:fs');
    const { tmpdir } = require('node:os');
    const { join } = require('node:path');
    const { openNamespace } = require(${JSON.stringify(packageDirectory)});
    const root = mkdtempSync(join(tmpdir(), 'zeppelin-node-leak-'));
    const store = openNamespace(root, 'leak', {
      vectorSpace: { dimensions: 128 },
    });
    try {
      const documents = Array.from({ length: 128 }, (_, index) => ({
        id: BigInt(index + 1),
        vector: new Float32Array(128).fill(index),
        text: 'result payload',
        metadata: new Uint8Array(64).fill(index),
      }));
      const ids = documents.map((document) => document.id);
      store.upsert(documents);
      const round = () => {
        store.get(ids);
        store.scan({ limit: 128 });
      };
      const collect = () => {
        global.gc();
        global.gc();
        return process.memoryUsage().rss;
      };
      for (let index = 0; index < 50; index += 1) round();
      collect();
      for (let index = 0; index < 300; index += 1) round();
      collect();
      for (let index = 0; index < 300; index += 1) round();
      const after = collect();
      for (let index = 0; index < 300; index += 1) round();
      const final = collect();
      const growth = final - after;
      assert.ok(
        growth < 16 * 1024 * 1024,
        'RSS grew by ' + growth + ' bytes in the final allocation window',
      );
      process.stdout.write(JSON.stringify({ growth }));
    } finally {
      store.close();
      rmSync(root, { force: true, recursive: true });
    }
  `;
  const child = spawnSync(process.execPath, ['--expose-gc', '-e', script], {
    encoding: 'utf8',
    timeout: 30_000,
  });
  assert.equal(child.status, 0, child.stderr || child.stdout);
  const { growth } = JSON.parse(child.stdout);
  assert.ok(growth < 16 * 1024 * 1024);
  t.diagnostic(`final-window RSS growth: ${growth} bytes`);
});
