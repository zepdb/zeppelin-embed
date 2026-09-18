/**
 * `Store.query`: the structured vector, lexical and hybrid surface.
 *
 * The parity test is the important one. It reads the same fixture the Rust
 * and C side generates and the Python and Swift bindings check
 * (`bindings/fixtures/cross_binding_parity_v1.json`, produced by
 * `crates/zeppelin-embed-ffi/tests/parity_fixture.rs`), indexes its corpus
 * through Node, and asserts the ids and the scores to the fixture's declared
 * precision. A ranking surface that merely "returns plausible hits" is not
 * evidence; agreeing digit for digit with the C ABI's own recorded answer is.
 *
 * The fixture opens its store with an explicit embedding epoch, which the
 * Node addon does not bind. A namespace with the same vector dimension and
 * the default tokenizer profile is used instead, and the scores agreeing
 * exactly is what shows that substitution changes nothing about ranking.
 */
import assert from 'node:assert/strict';
import { mkdtempSync, readFileSync, rmSync } from 'node:fs';
import { tmpdir } from 'node:os';
import { dirname, join, resolve } from 'node:path';
import { createRequire } from 'node:module';
import { fileURLToPath } from 'node:url';
import test from 'node:test';

const require = createRequire(import.meta.url);
const { CancellationToken, ZeppelinError, openNamespace } = require('..');

const packageDirectory = resolve(dirname(fileURLToPath(import.meta.url)), '..');
const fixture = JSON.parse(
  readFileSync(
    resolve(packageDirectory, '..', 'fixtures', 'cross_binding_parity_v1.json'),
    'utf8',
  ),
);

function temporaryRoot(prefix) {
  return mkdtempSync(join(tmpdir(), prefix));
}

function documentId(value) {
  return (BigInt(value.high) << 64n) + BigInt(value.low);
}

/** Rounds to the fixture's declared score precision before comparing. */
function rounded(value) {
  return Number(value.toFixed(fixture.score_precision));
}

function optionalRounded(value) {
  return value === undefined ? null : rounded(value);
}

test('query matches the C ABI parity fixture for vector, lexical, and hybrid', () => {
  assert.equal(fixture.schema, 'zeppelin-embed-cross-binding-parity');
  assert.equal(fixture.version, 1);

  const ingest = fixture.operations.find((operation) => operation.kind === 'ingest');
  const queries = fixture.operations.filter((operation) => operation.kind === 'query');
  assert.deepEqual(
    queries.map((operation) => operation.name),
    ['vector', 'lexical', 'hybrid'],
  );

  const root = temporaryRoot('zeppelin-node-query-parity-');
  const store = openNamespace(root, 'parity', {
    vectorSpace: { dimensions: fixture.epoch.document.dims },
  });
  try {
    store.upsert(
      ingest.documents.map((document) => ({
        id: documentId(document.doc_id),
        revision: BigInt(document.revision),
        timestamp: BigInt(document.timestamp),
        vector: new Float32Array(document.vector),
        text: document.text,
      })),
    );

    const modes = ['vector', 'lexical', 'hybrid'];
    for (const operation of queries) {
      const { vector, text, k, rules_enabled: rulesEnabled } = operation.request;
      const result = store.query({
        ...(vector === null ? {} : { vector: new Float32Array(vector) }),
        ...(text === null ? {} : { text }),
        k,
        rulesEnabled,
      });

      assert.equal(result.mode, modes[operation.expected.mode], operation.name);
      assert.equal(result.generation, BigInt(operation.expected.generation), operation.name);
      assert.deepEqual(
        result.hits.map((hit) => [
          hit.id,
          rounded(hit.score),
          optionalRounded(hit.vectorSquaredL2),
          optionalRounded(hit.lexicalBm25),
        ]),
        operation.expected.hits.map((hit) => [
          documentId(hit.doc_id),
          hit.score,
          hit.vector_squared_l2,
          hit.lexical_bm25,
        ]),
        operation.name,
      );
    }
  } finally {
    store.close();
    rmSync(root, { force: true, recursive: true });
  }
});

test('lastAsPrefix decides whether a partial term matches', () => {
  const root = temporaryRoot('zeppelin-node-query-prefix-');
  const store = openNamespace(root, 'records', {});
  try {
    store.upsert([
      { id: 1n, text: 'zeppelin airship over the harbour' },
      { id: 2n, text: 'harbour lights at dusk' },
      { id: 3n, text: 'quantized vectors and postings' },
    ]);

    const prefix = store.query({ text: 'harb', k: 5, lastAsPrefix: true });
    assert.equal(prefix.mode, 'lexical');
    assert.deepEqual(
      prefix.hits.map((hit) => hit.id).sort(),
      [1n, 2n],
    );

    // The same text without the flag is a whole term, which nothing carries.
    assert.deepEqual(store.query({ text: 'harb', k: 5 }).hits, []);
  } finally {
    store.close();
    rmSync(root, { force: true, recursive: true });
  }
});

test('a record-only namespace answers lexical queries and refuses hybrid ones', () => {
  const root = temporaryRoot('zeppelin-node-query-record-only-');
  const store = openNamespace(root, 'records', {});
  try {
    store.upsert([
      { id: 1n, text: 'harbour lights at dusk' },
      { id: 2n, text: 'quantized vectors and postings' },
    ]);

    const lexical = store.query({ text: 'harbour', k: 5 });
    assert.equal(lexical.mode, 'lexical');
    assert.deepEqual(
      lexical.hits.map((hit) => hit.id),
      [1n],
    );
    assert.equal(typeof lexical.hits[0].lexicalBm25, 'number');
    assert.equal(lexical.hits[0].vectorSquaredL2, undefined);
    assert.equal(lexical.fusion, undefined);

    assert.throws(
      () => store.query({ text: 'harbour', vector: new Float32Array([1, 0]), k: 5 }),
      (error) =>
        error instanceof ZeppelinError && error.code === 'ZE_ERR_NO_VECTOR_SPACE',
    );
  } finally {
    store.close();
    rmSync(root, { force: true, recursive: true });
  }
});

test('a hybrid query reports the fusion that produced its ranking', () => {
  const root = temporaryRoot('zeppelin-node-query-fusion-');
  const store = openNamespace(root, 'fused', { vectorSpace: { dimensions: 2 } });
  try {
    store.upsert([
      { id: 1n, vector: new Float32Array([1, 0]), text: 'harbour lights at dusk' },
      { id: 2n, vector: new Float32Array([0, 1]), text: 'quantized vectors' },
    ]);

    const result = store.query({
      text: 'harbour',
      vector: new Float32Array([1, 0]),
      k: 2,
      alpha: 0.75,
    });
    assert.equal(result.mode, 'hybrid');
    assert.notEqual(result.fusion, undefined);
    assert.equal(result.fusion.method, 'convex');
    assert.equal(result.fusion.effectiveAlpha, 0.75);
    assert.equal(typeof result.fusion.rounds, 'bigint');
    assert.equal(result.hits[0].id, 1n);
  } finally {
    store.close();
    rmSync(root, { force: true, recursive: true });
  }
});

test('a fired deadline and a cancelled token both stop the query', () => {
  const root = temporaryRoot('zeppelin-node-query-control-');
  const store = openNamespace(root, 'records', {});
  try {
    store.upsert([
      { id: 1n, text: 'harbour lights at dusk' },
      { id: 2n, text: 'zeppelin airship over the harbour' },
    ]);

    // A deadline this short has already expired by the time the scan starts,
    // so the engine reports it rather than returning a partial ranking.
    assert.throws(
      () => store.query({ text: 'harbour', k: 5, deadlineNs: 1n }),
      (error) =>
        error instanceof ZeppelinError && /deadline expired/.test(error.message),
    );

    // A deadline with real time in it completes normally.
    const completed = store.query({ text: 'harbour', k: 5, deadlineNs: 60_000_000_000n });
    assert.equal(completed.hits.length, 2);
    assert.equal(completed.budgetExhausted, false);

    const cancelled = new CancellationToken();
    cancelled.cancel();
    try {
      assert.throws(
        () => store.query({ text: 'harbour', k: 5, cancelToken: cancelled }),
        (error) =>
          error instanceof ZeppelinError && /cancelled/.test(error.message),
      );
    } finally {
      cancelled.close();
    }

    const live = new CancellationToken();
    try {
      assert.equal(store.query({ text: 'harbour', k: 5, cancelToken: live }).hits.length, 2);
    } finally {
      live.close();
    }

    // The C ABI takes a token or a deadline, never both.
    const both = new CancellationToken();
    try {
      assert.throws(
        () => store.query({ text: 'harbour', k: 5, cancelToken: both, deadlineNs: 1_000_000n }),
        (error) => error instanceof ZeppelinError,
      );
    } finally {
      both.close();
    }
  } finally {
    store.close();
    rmSync(root, { force: true, recursive: true });
  }
});

test('a query with no leg is refused rather than run empty', () => {
  const root = temporaryRoot('zeppelin-node-query-empty-');
  const store = openNamespace(root, 'records', {});
  try {
    assert.throws(
      () => store.query({ k: 5 }),
      (error) => error.code === 'ERR_MISSING_ARGS',
    );
    // A missing request and a string request are both the wrong shape; the
    // addon names the expected shape rather than guessing at a query.
    assert.throws(
      () => store.query(),
      (error) => error.code === 'ERR_INVALID_ARG_TYPE',
    );
    assert.throws(
      () => store.query('harbour'),
      (error) => error.code === 'ERR_INVALID_ARG_TYPE',
    );
    assert.throws(
      () => store.query({ text: 'harbour', tier: 'nonsense' }),
      (error) => error.code === 'ERR_OUT_OF_RANGE',
    );
  } finally {
    store.close();
    rmSync(root, { force: true, recursive: true });
  }
});

test('a closed cancellation token refuses further use', () => {
  const token = new CancellationToken();
  token.close();
  // Closing twice is a no-op; cancelling a closed token is refused by name.
  token.close();
  assert.throws(
    () => token.cancel(),
    (error) => error instanceof ZeppelinError && error.code === 'ZE_ERR_CLOSED',
  );
});
