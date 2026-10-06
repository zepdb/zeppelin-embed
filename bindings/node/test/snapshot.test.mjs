import assert from 'node:assert/strict';
import { mkdirSync, mkdtempSync, readdirSync, rmSync, writeFileSync } from 'node:fs';
import { tmpdir } from 'node:os';
import { join } from 'node:path';
import { createRequire } from 'node:module';
import { setImmediate as yieldToEventLoop } from 'node:timers/promises';
import test from 'node:test';

const require = createRequire(import.meta.url);
const { ZeppelinError, openNamespace } = require('..');

// Page-cache durability keeps the write loop fast; the snapshot itself always
// syncs its files.
const FAST = { durability: 'durable', commitTier: 'none' };
const SPEC = { attributes: [] };

function temporaryRoot(prefix) {
  return mkdtempSync(join(tmpdir(), prefix));
}

function text(id, revision) {
  return `meeting note ${id} revision ${revision}`;
}

/**
 * Applies writes to the store and records each one with the generation its
 * acknowledgement reported, so the state at any generation can be rebuilt.
 */
class Oracle {
  constructor(store) {
    this.store = store;
    this.log = [];
    this.ids = new Set();
  }

  upsert(id, revision) {
    const report = this.store.upsert([{ id, revision, text: text(id, revision) }]);
    this.log.push({ generation: report.generation, id, value: { revision, text: text(id, revision) } });
    this.ids.add(id);
    return report.generation;
  }

  delete(id) {
    const report = this.store.delete([id]);
    this.log.push({ generation: report.generation, id, value: null });
    return report.generation;
  }

  stateAt(generation) {
    const state = new Map([...this.ids].map((id) => [id, null]));
    for (const entry of this.log) {
      if (entry.generation <= generation) state.set(entry.id, entry.value);
    }
    return state;
  }
}

function contents(store, ids) {
  const documents = store.get(ids, { text: true }).documents;
  return new Map(
    ids.map((id, index) => {
      const document = documents[index];
      return [id, document === null ? null : { revision: document.revision, text: document.text }];
    }),
  );
}

async function rejectsWith(promise, code, pattern) {
  await assert.rejects(promise, (error) => {
    assert.ok(error instanceof ZeppelinError, `${error}`);
    assert.equal(error.code, code);
    if (pattern !== undefined) assert.match(error.message, pattern);
    return true;
  });
}

async function snapshotWhileWritingAttempt() {
  const root = temporaryRoot('zeppelin-node-snapshot-');
  const backups = temporaryRoot('zeppelin-node-snapshot-backups-');
  let store;
  try {
    store = openNamespace(root, 'notes', SPEC, FAST);
    const oracle = new Oracle(store);
    for (let batch = 0; batch < 3; batch += 1) {
      for (let id = 1; id <= 400; id += 1) oracle.upsert(BigInt(batch * 400 + id), 1n);
      store.seal();
    }
    for (let id = 1201; id <= 1250; id += 1) oracle.upsert(BigInt(id), 1n);
    oracle.delete(7n);

    let settled = false;
    const pending = store.snapshot(join(backups, 'backup')).finally(() => {
      settled = true;
    });
    // Keep writing through the same handle until the snapshot resolves:
    // deletes of sealed rows rewrite and unlink pinned segments, new rows
    // grow the WAL, and one seal publishes a new manifest.
    // A write that starts and ends while the staging directory exists ran
    // while the worker thread was copying.
    const copying = () => readdirSync(backups).some((name) => name.startsWith('.backup.snapshot-'));
    const overlapped = [];
    let next = 2000n;
    let writes = 0;
    while (!settled) {
      const before = copying();
      const acknowledged =
        writes % 3 === 0 ? oracle.delete(BigInt(writes + 10)) : oracle.upsert((next += 1n), 1n);
      if (before && copying()) overlapped.push(acknowledged);
      if (writes === 5) store.seal();
      writes += 1;
      await yieldToEventLoop();
    }
    const { generation } = await pending;
    assert.equal(typeof generation, 'bigint');

    if (overlapped.length === 0) return { accepted: false, writes };
    assert.ok(
      overlapped.every((acknowledged) => acknowledged > generation),
      'every write made during the copy is newer than the snapshot',
    );
    const expected = oracle.stateAt(generation);
    const ids = [...oracle.ids];
    assert.notDeepEqual(contents(store, ids), expected, 'the live store moved on');
    store.close();
    store = undefined;

    assert.deepEqual(readdirSync(backups), ['backup'], 'no staging directory remains');
    const readOnly = openNamespace(backups, 'backup', SPEC, { readOnly: true });
    assert.deepEqual(contents(readOnly, ids), expected);
    readOnly.close();

    const restored = openNamespace(backups, 'backup', SPEC, FAST);
    assert.deepEqual(contents(restored, ids), expected);
    restored.upsert([{ id: 5000n, text: 'written after restore' }]);
    restored.close();
    const reopened = openNamespace(backups, 'backup', SPEC, { readOnly: true });
    assert.equal(reopened.get([5000n], { text: true }).documents[0].text, 'written after restore');
    reopened.close();
    return { accepted: true, writes };
  } finally {
    store?.close();
    rmSync(root, { force: true, recursive: true });
    rmSync(backups, { force: true, recursive: true });
  }
}

test('a snapshot taken while the app keeps writing restores exactly its generation', async () => {
  const writesPerAttempt = [];
  for (let attempt = 0; attempt < 5; attempt += 1) {
    const { accepted, writes } = await snapshotWhileWritingAttempt();
    writesPerAttempt.push(writes);
    if (accepted) return;
  }
  assert.fail(
    `no write overlapped the copy after ${writesPerAttempt.length} attempts ` +
      `(writes per attempt: ${writesPerAttempt.join(', ')})`,
  );
});

test('snapshot rejects unusable targets and handles', async () => {
  const root = temporaryRoot('zeppelin-node-snapshot-reject-');
  const backups = temporaryRoot('zeppelin-node-snapshot-reject-backups-');
  let store;
  try {
    store = openNamespace(root, 'notes', SPEC, FAST);
    store.upsert([{ id: 1n, text: 'one' }]);

    const occupied = join(backups, 'occupied');
    mkdirSync(occupied);
    writeFileSync(join(occupied, 'keep.txt'), 'keep');
    await rejectsWith(store.snapshot(occupied), 'ZE_ERR_INVALID_ARGUMENT', /is not empty/);
    await rejectsWith(
      store.snapshot(join(root, 'notes', 'backup')),
      'ZE_ERR_INVALID_ARGUMENT',
      /is inside the store/,
    );
    await rejectsWith(
      store.snapshot(join(backups, 'missing', 'backup')),
      'ZE_ERR_INVALID_ARGUMENT',
      /parent directory does not exist/,
    );
    await rejectsWith(store.snapshot(''), 'ZE_ERR_INVALID_ARGUMENT', /must not be empty/);
    for (const target of [undefined, 42, null, { path: 'x' }]) {
      await assert.rejects(store.snapshot(target), TypeError, `target ${String(target)}`);
    }
    assert.deepEqual(readdirSync(backups), ['occupied'], 'no rejected snapshot wrote anything');
    assert.deepEqual(readdirSync(occupied), ['keep.txt']);

    // An existing empty directory is an accepted target.
    const empty = join(backups, 'empty');
    mkdirSync(empty);
    const { generation } = await store.snapshot(empty);
    assert.equal(generation, store.count().generation);
    store.close();

    await rejectsWith(store.snapshot(join(backups, 'closed')), 'ZE_ERR_CLOSED');
    store = openNamespace(root, 'notes', SPEC, { readOnly: true });
    await rejectsWith(store.snapshot(join(backups, 'read-only')), 'ZE_ERR_ACCESS_MODE');
  } finally {
    store?.close();
    rmSync(root, { force: true, recursive: true });
    rmSync(backups, { force: true, recursive: true });
  }
});
