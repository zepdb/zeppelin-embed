'use strict';

const assert = require('node:assert/strict');
const { mkdtempSync, rmSync } = require('node:fs');
const { tmpdir } = require('node:os');
const { join } = require('node:path');
const { ABI_VERSION, Store } = require('@zepdb/zeppelin-embed');

const directory = mkdtempSync(join(tmpdir(), 'zeppelin-node-installed-'));
const store = new Store(join(directory, 'index'));

try {
  store.ingest(
    [
      { id: 7n, vector: new Float32Array([1, 0]) },
      { id: 9n, vector: new Float32Array([0, 1]) },
    ],
    2,
  );
  const hits = store.search(new Float32Array([0.9, 0.1]), 1);
  assert.equal(ABI_VERSION, 1);
  assert.equal(hits.length, 1);
  assert.equal(hits[0].id, 7n);
} finally {
  store.close();
  rmSync(directory, { force: true, recursive: true });
}
