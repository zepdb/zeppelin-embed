import assert from 'node:assert/strict';
import { mkdtempSync, readdirSync, rmSync } from 'node:fs';
import { tmpdir } from 'node:os';
import { join } from 'node:path';
import { performance } from 'node:perf_hooks';
import { createRequire } from 'node:module';
import test from 'node:test';

const require = createRequire(import.meta.url);
const { ZeppelinError, openNamespace } = require('..');

// Page-cache durability keeps the timing gates about the engine, not fsync.
const FAST = { durability: 'durable', commitTier: 'none' };
const SPEC = { attributes: [] };

function temporaryRoot(prefix) {
  return mkdtempSync(join(tmpdir(), prefix));
}

function segmentFiles(root, name) {
  return readdirSync(join(root, name)).filter((file) => file.endsWith('.zseg'));
}

function segmentText(index) {
  return `segment ${index} the quick brown fox jumps over the lazy dog ${index % 97}`;
}

function upsertOneAtATime(store, count) {
  for (let index = 1; index <= count; index += 1) {
    store.upsert([{ id: BigInt(index), text: segmentText(index) }]);
  }
}

function timedReopen(root, name, options) {
  const started = performance.now();
  const store = openNamespace(root, name, SPEC, options);
  return { store, milliseconds: performance.now() - started };
}

test('seal absorbs the WAL and reopen reads the sealed segment', () => {
  const root = temporaryRoot('zeppelin-node-seal-');
  let store;
  try {
    store = openNamespace(root, 'notes', SPEC, FAST);
    upsertOneAtATime(store, 20);
    store.delete([3n]);
    const before = store.upsert([{ id: 2n, revision: 2n, text: 'harbour lights' }]);
    const report = store.seal();
    assert.equal(typeof report.generation, 'bigint');
    assert.ok(report.generation > before.generation);
    assert.equal(segmentFiles(root, 'notes').length, 1);
    store.close();

    store = openNamespace(root, 'notes', SPEC, FAST);
    assert.equal(store.count().count, 19n);
    assert.equal(store.get([3n]).documents[0], null);
    assert.equal(store.get([2n], { text: true }).documents[0].text, 'harbour lights');
    assert.deepEqual(
      store.query({ text: 'harbour', k: 5 }).hits.map((hit) => hit.id),
      [2n],
    );
  } finally {
    store?.close();
    rmSync(root, { force: true, recursive: true });
  }
});

test('sealing an empty active segment is a no-op', () => {
  const root = temporaryRoot('zeppelin-node-seal-empty-');
  let store;
  try {
    store = openNamespace(root, 'notes', SPEC, FAST);
    store.upsert([{ id: 1n, text: 'one' }]);
    const first = store.seal();
    assert.deepEqual(store.seal(), first);
    assert.equal(segmentFiles(root, 'notes').length, 1);
  } finally {
    store?.close();
    rmSync(root, { force: true, recursive: true });
  }
});

test('seal on a closed store reports ZE_ERR_CLOSED', () => {
  const root = temporaryRoot('zeppelin-node-seal-closed-');
  try {
    const store = openNamespace(root, 'notes', SPEC, FAST);
    store.close();
    assert.throws(
      () => store.seal(),
      (error) => error instanceof ZeppelinError && error.code === 'ZE_ERR_CLOSED',
    );
  } finally {
    rmSync(root, { force: true, recursive: true });
  }
});

test('autoSealRows refuses bad values before opening a handle', () => {
  const root = temporaryRoot('zeppelin-node-seal-options-');
  try {
    for (const autoSealRows of [0, -1, 1.5, Number.NaN, '10', 2 ** 53]) {
      assert.throws(
        () => openNamespace(root, 'notes', SPEC, { ...FAST, autoSealRows }),
        RangeError,
        `autoSealRows ${String(autoSealRows)}`,
      );
    }
    assert.throws(
      () => openNamespace(root, 'notes', SPEC, { readOnly: true, autoSealRows: 10 }),
      /readOnly/,
    );
    // No refused open left the writer lock held.
    openNamespace(root, 'notes', SPEC, FAST).close();
  } finally {
    rmSync(root, { force: true, recursive: true });
  }
});

test('autoSealRows seals at open and before the write past the threshold', () => {
  const root = temporaryRoot('zeppelin-node-seal-auto-');
  let store;
  try {
    store = openNamespace(root, 'notes', SPEC, FAST);
    upsertOneAtATime(store, 3);
    store.close();

    // The tail an earlier session left unsealed is sealed at open.
    store = openNamespace(root, 'notes', SPEC, { ...FAST, autoSealRows: 2 });
    assert.equal(segmentFiles(root, 'notes').length, 1);
    store.upsert([{ id: 10n, text: segmentText(10) }]);
    store.upsert([{ id: 11n, text: segmentText(11) }]);
    assert.equal(segmentFiles(root, 'notes').length, 1);
    store.delete([10n]);
    assert.equal(segmentFiles(root, 'notes').length, 2);
    store.upsert([{ id: 12n, text: segmentText(12) }]);
    assert.equal(segmentFiles(root, 'notes').length, 2);
    // An explicit seal restarts the count.
    store.seal();
    assert.equal(segmentFiles(root, 'notes').length, 3);
    store.upsert([{ id: 13n, text: segmentText(13) }]);
    store.upsert([{ id: 14n, text: segmentText(14) }]);
    assert.equal(segmentFiles(root, 'notes').length, 3);
    assert.equal(store.count().count, 7n);
  } finally {
    store?.close();
    rmSync(root, { force: true, recursive: true });
  }
});

// ZE-232: WAL replay was quadratic in unsealed rows (1.3 s for these 3,000
// documents on an M3 Max). It is now linear, about 25 ms there; the gate keeps
// better than 10x headroom.
test('reopens 3,000 unsealed texted documents in under 0.3 s', () => {
  const root = temporaryRoot('zeppelin-node-reopen-unsealed-');
  let store;
  try {
    store = openNamespace(root, 'segments', SPEC, FAST);
    upsertOneAtATime(store, 3000);
    store.close();
    const reopened = timedReopen(root, 'segments', FAST);
    store = reopened.store;
    assert.equal(store.count().count, 3000n);
    assert.ok(reopened.milliseconds < 300, `reopen took ${reopened.milliseconds.toFixed(1)} ms`);
  } finally {
    store?.close();
    rmSync(root, { force: true, recursive: true });
  }
});

// ZE-231: with autoSealRows, reopen reads sealed segments and replays at most
// one threshold of WAL, so it stays flat as the store grows (a few
// milliseconds for 10,000 single upserts on an M3 Max).
test('reopen stays flat with autoSealRows', () => {
  const root = temporaryRoot('zeppelin-node-reopen-sealed-');
  const options = { ...FAST, autoSealRows: 1000 };
  let store;
  try {
    store = openNamespace(root, 'segments', SPEC, options);
    upsertOneAtATime(store, 10_000);
    store.close();
    // Nine threshold seals while writing, and the tenth at the next open.
    assert.equal(segmentFiles(root, 'segments').length, 9);
    const reopened = timedReopen(root, 'segments', options);
    store = reopened.store;
    assert.equal(segmentFiles(root, 'segments').length, 10);
    assert.equal(store.count().count, 10_000n);
    assert.ok(reopened.milliseconds < 300, `reopen took ${reopened.milliseconds.toFixed(1)} ms`);
    assert.equal(store.query({ text: 'fox', k: 3 }).hits.length, 3);
  } finally {
    store?.close();
    rmSync(root, { force: true, recursive: true });
  }
});
