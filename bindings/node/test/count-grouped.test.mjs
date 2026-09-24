import assert from 'node:assert/strict';
import { mkdtempSync, rmSync } from 'node:fs';
import { tmpdir } from 'node:os';
import { join } from 'node:path';
import { createRequire } from 'node:module';
import test from 'node:test';

const require = createRequire(import.meta.url);
const { ZeppelinError, openNamespace } = require('..');

const FOLDER = 1;
const SPEAKER = 2;
const DAY = 3;
const SIZE = 4;
const SCORE = 5;
const SPEC = {
  attributes: [
    { id: FOLDER, name: 'folder', type: 'dictionaryString', nullable: true },
    { id: SPEAKER, name: 'speaker', type: 'rawString', nullable: true },
    { id: DAY, name: 'day', type: 'i64', nullable: true },
    { id: SIZE, name: 'size', type: 'u64', nullable: true },
    { id: SCORE, name: 'score', type: 'f64', nullable: true },
  ],
};

function note(id, { folder, speaker, day, size, revision = 1n }) {
  const attributes = [];
  if (folder !== undefined) attributes.push({ id: FOLDER, type: 'string', value: folder });
  if (speaker !== undefined) attributes.push({ id: SPEAKER, type: 'string', value: speaker });
  if (day !== undefined) attributes.push({ id: DAY, type: 'i64', value: day });
  if (size !== undefined) attributes.push({ id: SIZE, type: 'u64', value: size });
  return { id, revision, timestamp: id, text: `note ${id}`, attributes };
}

function withStore(prefix, body) {
  const root = mkdtempSync(join(tmpdir(), prefix));
  const store = openNamespace(root, 'notes', SPEC);
  try {
    body(store);
  } finally {
    store.close();
    rmSync(root, { force: true, recursive: true });
  }
}

test('count groups by an attribute across sealed and active rows at one generation', () => {
  withStore('zeppelin-node-count-grouped-', (store) => {
    store.upsert([
      note(1n, { folder: 'work', speaker: 'ana', day: 3n, size: 9n }),
      note(2n, { folder: 'home', speaker: 'bo', day: -1n }),
      note(3n, {}),
    ]);
    store.seal();
    store.upsert([
      note(1n, { folder: 'archive', speaker: 'ana', day: 3n, revision: 2n }),
      note(4n, { folder: 'work', speaker: 'ana', size: 9n }),
      note(5n, { folder: '', day: 3n, size: 2n }),
    ]);
    store.delete([2n]);

    const plain = store.count();
    const folders = store.count({ groupBy: { attributeId: FOLDER } });
    assert.deepEqual(folders, {
      count: 4n,
      generation: plain.generation,
      groups: [
        { value: '', count: 1n },
        { value: 'archive', count: 1n },
        { value: 'work', count: 1n },
      ],
      missingCount: 1n,
    });

    const speakers = store.count({ groupBy: { attributeId: SPEAKER, limit: 2 } });
    assert.deepEqual(speakers.groups, [{ value: 'ana', count: 2n }]);
    assert.equal(speakers.missingCount, 2n);

    const days = store.count({
      groupBy: { attributeId: DAY },
      filter: { op: 'exists', attributeId: SIZE },
    });
    assert.deepEqual(days.groups, [{ value: 3n, count: 1n }]);
    assert.equal(days.missingCount, 1n);
    assert.equal(days.count, store.count({ filter: { op: 'exists', attributeId: SIZE } }).count);

    const sizes = store.count({
      groupBy: { attributeId: SIZE },
      timestampRange: { start: 4n, end: 6n },
    });
    assert.deepEqual(sizes.groups, [
      { value: 2n, count: 1n },
      { value: 9n, count: 1n },
    ]);
    assert.equal(sizes.missingCount, 0n);

    for (const group of folders.groups) {
      const filtered = store.count({
        filter: {
          op: 'eq',
          attributeId: FOLDER,
          values: [{ id: FOLDER, type: 'string', value: group.value }],
        },
      });
      assert.equal(filtered.count, group.count);
    }
  });
});

test('grouped count rejects bad group requests with precise errors', () => {
  withStore('zeppelin-node-count-grouped-errors-', (store) => {
    store.upsert([
      note(1n, { folder: 'a' }),
      note(2n, { folder: 'b' }),
      note(3n, { folder: 'c' }),
    ]);
    const engineError = (code, message) => (error) =>
      error instanceof ZeppelinError &&
      error.code === code &&
      error.message.includes(message);

    assert.throws(
      () => store.count({ groupBy: { attributeId: FOLDER, limit: 2 } }),
      engineError('ZE_ERR_BUDGET_EXCEEDED', 'more than 2 distinct values'),
    );
    assert.throws(
      () => store.count({ groupBy: { attributeId: SCORE } }),
      engineError('ZE_ERR_INVALID_ARGUMENT', 'group-by attribute 5 has type F64'),
    );
    assert.throws(
      () => store.count({ groupBy: { attributeId: 99 } }),
      engineError('ZE_ERR_INVALID_ARGUMENT', 'group-by attribute 99 is not in the schema'),
    );
    for (const limit of [0, 65537, 1.5, -1, Number.NaN]) {
      assert.throws(
        () => store.count({ groupBy: { attributeId: FOLDER, limit } }),
        { name: 'RangeError', message: 'groupBy.limit must be an integer in 1..=65536' },
      );
    }
    assert.throws(
      () => store.count({ groupBy: { attributeId: FOLDER, limit: '4' } }),
      { name: 'TypeError', message: 'groupBy.limit must be a number' },
    );
    for (const attributeId of [-1, 1.5, 2 ** 32, '1']) {
      assert.throws(
        () => store.count({ groupBy: { attributeId } }),
        /groupBy\.attributeId must be/,
      );
    }
    assert.throws(() => store.count({ groupBy: {} }), {
      name: 'TypeError',
      message: 'groupBy.attributeId is required',
    });
    assert.throws(() => store.count({ groupBy: 1 }), {
      name: 'TypeError',
      message: 'groupBy must be an object',
    });
  });
});
