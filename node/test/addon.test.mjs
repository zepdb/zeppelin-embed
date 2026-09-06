import assert from 'node:assert/strict';
import { mkdtempSync, rmSync } from 'node:fs';
import { tmpdir } from 'node:os';
import { join } from 'node:path';
import { createRequire } from 'node:module';
import test from 'node:test';

const require = createRequire(import.meta.url);
const { ABI_VERSION, Store, ZeppelinError } = require('..');

test('ingests and searches caller-supplied vectors through the C ABI', () => {
  const directory = mkdtempSync(join(tmpdir(), 'zeppelin-node-test-'));
  const store = new Store(join(directory, 'index'));
  try {
    const mutation = store.ingest(
      [
        { id: 1n, vector: new Float32Array([0.9, 0.1, 0.05, 0]) },
        { id: (1n << 80n) + 2n, vector: new Float32Array([0.1, 0.9, 0.05, 0]) },
      ],
      4,
    );
    assert.equal(ABI_VERSION, 1);
    assert.equal(typeof mutation.generation, 'bigint');

    const hits = store.search(new Float32Array([0.88, 0.12, 0.07, 0.02]), 2);
    assert.equal(hits.length, 2);
    assert.equal(hits[0].id, 1n);
    assert.equal(typeof hits[0].score, 'number');
  } finally {
    store.close();
    rmSync(directory, { force: true, recursive: true });
  }
});

test('surfaces C ABI failures as ZeppelinError instances', () => {
  const directory = mkdtempSync(join(tmpdir(), 'zeppelin-node-error-'));
  const store = new Store(join(directory, 'index'));
  try {
    assert.throws(
      () =>
        store.ingest(
          [{ id: 1n, vector: new Float32Array([0.9, 0.1, 0.05, 0]) }],
          3,
        ),
      (error) =>
        error instanceof ZeppelinError &&
        error.code === 'ZE_ERR_DIMENSION_MISMATCH' &&
        error.errorCode === 18,
    );
  } finally {
    store.close();
    rmSync(directory, { force: true, recursive: true });
  }
});

test('releases the handle and reports use after close', () => {
  const directory = mkdtempSync(join(tmpdir(), 'zeppelin-node-close-'));
  const store = new Store(join(directory, 'index'));
  store.close();
  try {
    assert.throws(
      () => store.search(new Float32Array([1, 0]), 1),
      (error) =>
        error instanceof ZeppelinError && error.code === 'ZE_ERR_CLOSED',
    );
  } finally {
    rmSync(directory, { force: true, recursive: true });
  }
});
