import assert from 'node:assert/strict';
import { mkdtempSync, readdirSync, readFileSync, rmSync } from 'node:fs';
import { tmpdir } from 'node:os';
import { join } from 'node:path';
import { createRequire } from 'node:module';
import test from 'node:test';

const require = createRequire(import.meta.url);
const { ZeppelinError, openNamespace } = require('..');

const MARKER = 'ZE217NODEPURGEDMARKER';
const SURVIVOR = 'ZE217NODESURVIVORMARKER';
const spec = { attributes: [{ id: 1, name: 'noteId', type: 'u64' }] };

function temporaryRoot() {
  return mkdtempSync(join(tmpdir(), 'zeppelin-node-delete-where-'));
}

function noteIs(note) {
  return { op: 'eq', attributeId: 1, values: [{ id: 1, type: 'u64', value: note }] };
}

function segment(id, note, text) {
  return { id, text, attributes: [{ id: 1, type: 'u64', value: note }] };
}

/** Note 7 has two sealed and two unsealed segments; note 8 has two. */
function populate(store) {
  store.upsert([
    segment(1n, 7n, `${MARKER} sealed one`),
    segment(2n, 7n, `${MARKER} sealed two`),
    segment(3n, 8n, `${SURVIVOR} sealed`),
  ]);
  store.seal();
  store.upsert([
    segment(4n, 7n, `${MARKER} unsealed one`),
    segment(5n, 7n, `${MARKER} unsealed two`),
    segment(6n, 8n, 'unsealed survivor'),
  ]);
}

function filesContaining(directory, needle) {
  const hits = [];
  for (const entry of readdirSync(directory, { withFileTypes: true })) {
    const path = join(directory, entry.name);
    if (entry.isDirectory()) {
      hits.push(...filesContaining(path, needle));
    } else if (readFileSync(path).includes(needle)) {
      hits.push(path);
    }
  }
  return hits;
}

function markerFiles(root) {
  return [
    ...filesContaining(root, MARKER),
    ...filesContaining(root, MARKER.toLowerCase()),
  ];
}

function isZeppelin(code) {
  return (error) => error instanceof ZeppelinError && error.code === code;
}

test('deleteWhere removes every match at once and keeps the rest', () => {
  const root = temporaryRoot();
  let store = openNamespace(root, 'notes', spec);
  try {
    populate(store);
    const before = store.count().generation;

    const report = store.deleteWhere(noteIs(7n));

    assert.equal(report.deleted, 4n);
    assert.ok(report.generation > before);
    assert.equal(store.count().generation, report.generation);
    assert.equal(store.count({ filter: noteIs(7n) }).count, 0n);
    assert.equal(store.count({ filter: noteIs(8n) }).count, 2n);
    assert.equal(store.get([1n, 2n, 4n, 5n]).missingCount, 4);
    store.close();
    store = openNamespace(root, 'notes', spec);
    assert.equal(store.count({ filter: noteIs(7n) }).count, 0n);
    assert.equal(store.count().count, 2n);
  } finally {
    store.close();
    rmSync(root, { force: true, recursive: true });
  }
});

test('deleteWhere leaves no deleted text in any store file', () => {
  const root = temporaryRoot();
  const store = openNamespace(root, 'notes', spec);
  try {
    populate(store);
    assert.notDeepEqual(markerFiles(root), [], 'the marker is findable first');

    store.deleteWhere(noteIs(7n));

    assert.deepEqual(markerFiles(root), []);
    assert.notDeepEqual(filesContaining(root, SURVIVOR), []);
  } finally {
    store.close();
    rmSync(root, { force: true, recursive: true });
  }
});

test('deleteWhere with no match deletes nothing at the same generation', () => {
  const root = temporaryRoot();
  const store = openNamespace(root, 'notes', spec);
  try {
    populate(store);
    const before = store.count().generation;

    const report = store.deleteWhere(noteIs(99n));

    assert.deepEqual(report, { deleted: 0n, generation: before });
    assert.equal(store.count().count, 6n);
  } finally {
    store.close();
    rmSync(root, { force: true, recursive: true });
  }
});

test('deleteWhere rejects a missing or invalid filter and deletes nothing', () => {
  const root = temporaryRoot();
  const store = openNamespace(root, 'notes', spec);
  try {
    populate(store);
    assert.throws(() => store.deleteWhere(), {
      name: 'TypeError',
      code: 'ERR_MISSING_ARGS',
    });
    assert.throws(() => store.deleteWhere(null), {
      name: 'TypeError',
      code: 'ERR_MISSING_ARGS',
    });
    assert.throws(() => store.deleteWhere('noteId = 7'), {
      name: 'TypeError',
      code: 'ERR_INVALID_ARG_TYPE',
    });
    assert.throws(() => store.deleteWhere({ op: 'like', attributeId: 1 }), {
      name: 'RangeError',
      code: 'ERR_OUT_OF_RANGE',
    });
    assert.throws(
      () => store.deleteWhere({ ...noteIs(7n), attributeId: 9 }),
      isZeppelin('ZE_ERR_INVALID_ARGUMENT'),
    );
    assert.throws(
      () =>
        store.deleteWhere({
          op: 'eq',
          attributeId: 1,
          values: [{ id: 1, type: 'string', value: '7' }],
        }),
      isZeppelin('ZE_ERR_INVALID_ARGUMENT'),
    );
    assert.equal(store.count().count, 6n);
  } finally {
    store.close();
    rmSync(root, { force: true, recursive: true });
  }
});

test('deleteWhere on a read-only store throws ZE_ERR_ACCESS_MODE', () => {
  const root = temporaryRoot();
  let store = openNamespace(root, 'notes', spec);
  try {
    populate(store);
    store.close();
    store = openNamespace(root, 'notes', spec, { readOnly: true });

    assert.throws(() => store.deleteWhere(noteIs(7n)), isZeppelin('ZE_ERR_ACCESS_MODE'));
    assert.equal(store.count({ filter: noteIs(7n) }).count, 4n);
  } finally {
    store.close();
    rmSync(root, { force: true, recursive: true });
  }
});

for (const sealBeforeDelete of [false, true]) {
  test(`deleteWhere after replacement and delete (seal=${sealBeforeDelete})`, () => {
    const root = temporaryRoot();
    const durable = { durability: 'durable', commitTier: 'durable' };
    let store = openNamespace(root, 'notes', spec, durable);
    try {
      store.upsert([segment(1n, 7n, MARKER)]);
      store.seal();
      store.upsert([segment(2n, 7n, 'currenttranscript')]);
      store.delete([1n]);
      if (sealBeforeDelete) store.seal();
      assert.equal(store.deleteWhere(noteIs(7n)).deleted, 1n);
      store.upsert([segment(3n, 8n, SURVIVOR)]);
      assert.notDeepEqual(markerFiles(root), []);
      const purged = store.purge([1n]);
      assert.equal(purged.segmentsRewritten, 1n);
      assert.equal(purged.unknownIdCount, 0n);
      assert.equal(purged.walRewritten, true);
      assert.equal(purged.isNoOp, false);
      assert.equal(purged.generation, store.count().generation);
      assert.deepEqual(markerFiles(root), []);
      assert.deepEqual(filesContaining(root, 'currenttranscript'), []);
      assert.equal(store.count({ filter: noteIs(8n) }).count, 1n);
      store.close();
      store = openNamespace(root, 'notes', spec, durable);
      assert.equal(store.count({ filter: noteIs(7n) }).count, 0n);
      assert.equal(store.get([3n], { text: true }).documents[0].text, SURVIVOR);
      assert.equal(store.purge([999n]).isNoOp, true);
    } finally {
      store.close();
      rmSync(root, { recursive: true, force: true });
    }
  });
}
