'use strict';

const assert = require('node:assert/strict');
const { mkdtempSync, rmSync } = require('node:fs');
const { tmpdir } = require('node:os');
const { join } = require('node:path');
const {
  ABI_VERSION,
  CancellationToken,
  Store,
  openNamespace,
} = require('@zepdb/zeppelin-embed');

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
}

// The text and hybrid surface, through the installed package rather than the
// source tree: a lexical query, a hybrid one, and a cancellation token.
const records = openNamespace(join(directory, 'records'), 'notes', {
  vectorSpace: { dimensions: 2 },
});
const token = new CancellationToken();
try {
  records.upsert([
    { id: 1n, vector: new Float32Array([1, 0]), text: 'harbour lights at dusk' },
    { id: 2n, vector: new Float32Array([0, 1]), text: 'quantized vectors' },
  ]);

  const lexical = records.query({ text: 'harbour', k: 5, cancelToken: token });
  assert.equal(lexical.mode, 'lexical');
  assert.deepEqual(
    lexical.hits.map((hit) => hit.id),
    [1n],
  );
  assert.equal(typeof lexical.hits[0].lexicalBm25, 'number');

  const hybrid = records.query({
    text: 'harbour',
    vector: new Float32Array([1, 0]),
    k: 2,
  });
  assert.equal(hybrid.mode, 'hybrid');
  assert.equal(hybrid.hits[0].id, 1n);
  assert.notEqual(hybrid.fusion, undefined);

  const prefix = records.query({ text: 'harb', k: 5, lastAsPrefix: true });
  assert.deepEqual(
    prefix.hits.map((hit) => hit.id),
    [1n],
  );
} finally {
  token.close();
  records.close();
  rmSync(directory, { force: true, recursive: true });
}
