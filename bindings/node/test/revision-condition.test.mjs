import assert from 'node:assert/strict';
import { mkdtempSync, rmSync, statSync } from 'node:fs';
import { tmpdir } from 'node:os';
import { join } from 'node:path';
import { setImmediate as yieldTurn } from 'node:timers/promises';
import { createRequire } from 'node:module';
import test from 'node:test';

const require = createRequire(import.meta.url);
const { ZeppelinError, openNamespace } = require('..');

/** Opens a record-only namespace, runs `body`, and always cleans up. */
function withStore(body) {
  const root = mkdtempSync(join(tmpdir(), 'zeppelin-node-revision-'));
  const store = openNamespace(root, 'notes', { attributes: [] });
  const walBytes = () => statSync(join(root, 'notes', 'wal.ze')).size;
  try {
    return body(store, walBytes);
  } finally {
    store.close();
    rmSync(root, { force: true, recursive: true });
  }
}

async function withStoreAsync(body) {
  const root = mkdtempSync(join(tmpdir(), 'zeppelin-node-revision-'));
  const store = openNamespace(root, 'notes', { attributes: [] });
  try {
    return await body(store);
  } finally {
    store.close();
    rmSync(root, { force: true, recursive: true });
  }
}

function liveRevision(store, id) {
  return store.get([id]).documents[0]?.revision ?? null;
}

function assertConflict(write, expected) {
  assert.throws(write, (error) => {
    assert.ok(error instanceof ZeppelinError);
    assert.equal(error.code, 'ZE_ERR_REVISION_CONFLICT');
    assert.deepEqual(error.conflict, expected);
    return true;
  });
}

test('a matching expectedRevision commits the upsert', () => {
  withStore((store) => {
    store.upsert([{ id: 1n, revision: 4n, text: 'head' }]);
    const report = store.upsert([
      { id: 1n, revision: 5n, expectedRevision: 4n, text: 'head v5' },
    ]);
    assert.equal(liveRevision(store, 1n), 5n);
    assert.equal(store.get([1n]).generation, report.generation);
  });
});

test('a mismatched expectedRevision throws and writes nothing', () => {
  withStore((store, walBytes) => {
    store.upsert([{ id: 1n, revision: 3n }]);
    const before = { generation: store.get([1n]).generation, wal: walBytes() };
    assertConflict(
      () => store.upsert([{ id: 1n, revision: 4n, expectedRevision: 2n }]),
      { index: 0, id: 1n, expectedRevision: 2n, currentRevision: 3n },
    );
    assert.deepEqual(
      { generation: store.get([1n]).generation, wal: walBytes() },
      before,
    );
    assert.equal(liveRevision(store, 1n), 3n);
  });
});

test('null expectedRevision rejects a live id and accepts unknown and deleted ids', () => {
  withStore((store) => {
    store.upsert([{ id: 1n }, { id: 2n, revision: 5n }]);
    store.delete([2n]);
    assertConflict(
      () => store.upsert([{ id: 1n, revision: 2n, expectedRevision: null }]),
      { index: 0, id: 1n, expectedRevision: null, currentRevision: 1n },
    );
    store.upsert([
      { id: 3n, expectedRevision: null },
      { id: 2n, revision: 6n, expectedRevision: null },
    ]);
    assert.equal(liveRevision(store, 3n), 1n);
    assert.equal(liveRevision(store, 2n), 6n);
  });
});

test('one failed condition aborts the whole batch', () => {
  withStore((store, walBytes) => {
    store.upsert([{ id: 1n }, { id: 2n }]);
    const before = { generation: store.get([1n]).generation, wal: walBytes() };
    assertConflict(
      () =>
        store.upsert([
          { id: 1n, revision: 2n, expectedRevision: 1n },
          { id: 3n },
          { id: 2n, revision: 2n, expectedRevision: 7n },
        ]),
      { index: 2, id: 2n, expectedRevision: 7n, currentRevision: 1n },
    );
    assert.deepEqual(
      { generation: store.get([1n]).generation, wal: walBytes() },
      before,
    );
    assert.equal(liveRevision(store, 1n), 1n);
    assert.equal(liveRevision(store, 3n), null);
  });
});

test('delete honours per-entry expectedRevision', () => {
  withStore((store, walBytes) => {
    store.upsert([{ id: 1n, revision: 2n }, { id: 2n, revision: 4n }]);
    const before = { generation: store.get([1n]).generation, wal: walBytes() };
    assertConflict(
      () =>
        store.delete([
          { id: 1n, expectedRevision: 2n },
          { id: 2n, expectedRevision: 3n },
        ]),
      { index: 1, id: 2n, expectedRevision: 3n, currentRevision: 4n },
    );
    assert.deepEqual(
      { generation: store.get([1n]).generation, wal: walBytes() },
      before,
    );

    store.delete([{ id: 1n, expectedRevision: 2n }, 2n, { id: 9n, expectedRevision: null }]);
    assert.equal(liveRevision(store, 1n), null);
    assert.equal(liveRevision(store, 2n), null);
    assertConflict(
      () => store.delete([{ id: 1n, expectedRevision: 2n }]),
      { index: 0, id: 1n, expectedRevision: 2n, currentRevision: null },
    );
  });
});

test('expectedRevision and delete targets are validated', () => {
  withStore((store) => {
    const rejects = (write, ErrorType, code) =>
      assert.throws(write, (error) => error instanceof ErrorType && error.code === code);
    for (const expectedRevision of ['1', 1, true, {}]) {
      rejects(
        () => store.upsert([{ id: 1n, expectedRevision }]),
        TypeError,
        'ERR_INVALID_ARG_TYPE',
      );
      rejects(
        () => store.delete([{ id: 1n, expectedRevision }]),
        TypeError,
        'ERR_INVALID_ARG_TYPE',
      );
    }
    for (const expectedRevision of [-1n, 1n << 64n]) {
      rejects(
        () => store.upsert([{ id: 1n, expectedRevision }]),
        RangeError,
        'ERR_OUT_OF_RANGE',
      );
    }
    rejects(() => store.delete([{ expectedRevision: 1n }]), TypeError, 'ERR_MISSING_ARGS');
    rejects(() => store.delete([{ id: 1 }]), TypeError, 'ERR_INVALID_ARG_TYPE');
    rejects(() => store.delete([null]), TypeError, 'ERR_INVALID_ARG_TYPE');
    assert.equal(liveRevision(store, 1n), null);

    store.upsert([{ id: 1n, expectedRevision: undefined }]);
    assert.equal(liveRevision(store, 1n), 1n);
  });
});

test('interleaved async compare-and-set writers never lose an update', async () => {
  const writers = 8;
  const increments = 20;
  await withStoreAsync(async (store) => {
    store.upsert([{ id: 1n, revision: 0n }]);
    let conflicts = 0;
    const writer = async () => {
      for (let applied = 0; applied < increments; ) {
        const current = liveRevision(store, 1n);
        // Every writer reads before any of them writes.
        await yieldTurn();
        try {
          store.upsert([{ id: 1n, revision: current + 1n, expectedRevision: current }]);
          applied += 1;
        } catch (error) {
          if (error.code !== 'ZE_ERR_REVISION_CONFLICT') throw error;
          conflicts += 1;
        }
      }
    };
    await Promise.all(Array.from({ length: writers }, writer));
    assert.equal(liveRevision(store, 1n), BigInt(writers * increments));
    assert.ok(conflicts > 0, 'the writers never raced');
  });
});
