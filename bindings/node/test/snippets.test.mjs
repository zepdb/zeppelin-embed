/**
 * `Store.query({ snippetBytes })`: per-hit excerpts whose highlighted ranges
 * come from the engine's own analyzer and the terms the query scored (ZE-215).
 *
 * Every highlight is checked by slicing `snippet.text` with the returned
 * range, so a byte offset handed to JavaScript unconverted, or a range the
 * engine did not match, fails as the wrong substring rather than passing.
 */
import assert from 'node:assert/strict';
import { mkdtempSync, rmSync } from 'node:fs';
import { tmpdir } from 'node:os';
import { join } from 'node:path';
import { createRequire } from 'node:module';
import test from 'node:test';

const require = createRequire(import.meta.url);
const { ZeppelinError, openNamespace } = require('..');

function withStore(prefix, spec, documents, body) {
  const root = mkdtempSync(join(tmpdir(), prefix));
  const store = openNamespace(root, 'notes', spec);
  try {
    store.upsert(documents);
    body(store);
  } finally {
    store.close();
    rmSync(root, { force: true, recursive: true });
  }
}

function marked(hit) {
  assert.notEqual(hit.snippet, undefined, `hit ${hit.id} has a snippet`);
  return hit.snippet.highlights.map(({ start, end }) => hit.snippet.text.slice(start, end));
}

function hit(result, id) {
  const found = result.hits.find((candidate) => candidate.id === id);
  assert.notEqual(found, undefined, `hit ${id}`);
  return found;
}

test('highlights are UTF-16 ranges into the excerpt across accents and emoji', () => {
  withStore(
    'zeppelin-node-snippets-utf16-',
    {},
    [
      // A surrogate pair and a two-byte é sit between the matches, so byte
      // offsets and UTF-16 offsets disagree after the first highlight.
      { id: 1n, text: 'Harbours 🚀 café and a 👩‍🚀 at the harbour' },
      { id: 2n, text: 'nothing relevant' },
    ],
    (store) => {
      const result = store.query({ text: 'harbour cafe', k: 5, snippetBytes: 200 });
      assert.equal(result.hits.length, 1);
      const only = hit(result, 1n);
      assert.deepEqual(marked(only), ['Harbours', 'café', 'harbour']);
      assert.equal(only.snippet.text, 'Harbours 🚀 café and a 👩‍🚀 at the harbour');
      assert.equal(only.snippet.truncatedStart, false);
      assert.equal(only.snippet.truncatedEnd, false);
    },
  );
});

test('CJK text is highlighted where the engine matched it', () => {
  withStore(
    'zeppelin-node-snippets-cjk-',
    {},
    [{ id: 1n, text: '東京 と 中文 の テスト' }],
    (store) => {
      const result = store.query({ text: '中文', k: 5, snippetBytes: 64 });
      assert.equal(result.hits.length, 1);
      const marks = marked(result.hits[0]);
      assert.ok(marks.length > 0);
      for (const mark of marks) assert.ok('中文'.includes(mark), `unexpected mark ${mark}`);
    },
  );
});

test('a prefix query marks every term the prefix expanded to', () => {
  withStore(
    'zeppelin-node-snippets-prefix-',
    {},
    [
      { id: 1n, text: 'harbour and harbinger' },
      { id: 2n, text: 'the harp' },
    ],
    (store) => {
      const result = store.query({ text: 'harb', k: 5, lastAsPrefix: true, snippetBytes: 64 });
      assert.deepEqual(result.hits.map((candidate) => candidate.id), [1n]);
      assert.deepEqual(marked(result.hits[0]), ['harbour', 'harbinger']);
    },
  );
});

test('a hybrid hit the lexical leg did not match carries no snippet', () => {
  withStore(
    'zeppelin-node-snippets-hybrid-',
    { vectorSpace: { dimensions: 2 } },
    [
      { id: 1n, vector: new Float32Array([1, 0]), text: 'harbour lights at dusk' },
      { id: 2n, vector: new Float32Array([0.9, 0.1]), text: 'quantized vectors' },
    ],
    (store) => {
      const result = store.query({
        text: 'harbour',
        vector: new Float32Array([1, 0]),
        k: 2,
        snippetBytes: 64,
      });
      assert.equal(result.mode, 'hybrid');
      assert.deepEqual(marked(hit(result, 1n)), ['harbour']);
      assert.equal(hit(result, 2n).snippet, undefined);
    },
  );
});

test('the excerpt is bounded by snippetBytes and reports where it was cut', () => {
  const text = `${'alpha '.repeat(40)}harbour lights${' omega'.repeat(40)}`;
  withStore('zeppelin-node-snippets-window-', {}, [{ id: 1n, text }], (store) => {
    const { snippet } = store.query({ text: 'harbour', k: 1, snippetBytes: 16 }).hits[0];
    assert.ok(Buffer.byteLength(snippet.text) <= 16 + 3, snippet.text);
    assert.ok(snippet.text.startsWith('harbour'));
    assert.equal(snippet.truncatedStart, true);
    assert.equal(snippet.truncatedEnd, true);
    assert.deepEqual(snippet.highlights, [{ start: 0, end: 7, sourceByteStart: 240, sourceByteEnd: 247 }]);

    // A window reaching the end is cut only at the start.
    const tail = store.query({ text: 'harbour', k: 1, snippetBytes: text.length }).hits[0];
    assert.equal(tail.snippet.truncatedStart, true);
    assert.equal(tail.snippet.truncatedEnd, false);
    assert.ok(text.endsWith(tail.snippet.text));
  });
});

test('without snippetBytes a query returns the same hits and no snippet', () => {
  withStore(
    'zeppelin-node-snippets-default-',
    {},
    [
      { id: 1n, text: 'harbour lights at dusk' },
      { id: 2n, text: 'the harbour master and the harbour' },
    ],
    (store) => {
      const plain = store.query({ text: 'harbour', k: 5 });
      const withSnippets = store.query({ text: 'harbour', k: 5, snippetBytes: 32 });
      assert.equal(plain.hits.length, 2);
      for (const candidate of plain.hits) assert.equal('snippet' in candidate, false);
      assert.deepEqual(
        withSnippets.hits.map(({ snippet, ...rest }) => rest),
        plain.hits,
      );
      assert.ok(withSnippets.hits.every((candidate) => candidate.snippet !== undefined));
    },
  );
});

test('snippetBytes is validated at the binding and at the engine', () => {
  withStore(
    'zeppelin-node-snippets-invalid-',
    { vectorSpace: { dimensions: 2 } },
    [{ id: 1n, vector: new Float32Array([1, 0]), text: 'harbour' }],
    (store) => {
      for (const snippetBytes of [0, -1, 1.5, 2 ** 32, Number.NaN, Number.POSITIVE_INFINITY]) {
        assert.throws(
          () => store.query({ text: 'harbour', snippetBytes }),
          (error) => error instanceof RangeError && error.code === 'ERR_OUT_OF_RANGE',
          `snippetBytes ${snippetBytes}`,
        );
      }
      for (const snippetBytes of ['64', 64n, null, {}]) {
        assert.throws(
          () => store.query({ text: 'harbour', snippetBytes }),
          (error) => error instanceof TypeError && error.code === 'ERR_INVALID_ARG_TYPE',
          `snippetBytes ${String(snippetBytes)}`,
        );
      }
      // Snippets need the lexical leg; the engine refuses a vector-only query.
      assert.throws(
        () => store.query({ vector: new Float32Array([1, 0]), snippetBytes: 64 }),
        (error) => error instanceof ZeppelinError && error.code === 'ZE_ERR_INVALID_ARGUMENT',
      );
    },
  );
});

test('absolute source byte ranges survive multibyte prefixes in both query paths', () => {
  const text = '前 🚀 café harbour café tail';
  withStore('zeppelin-node-source-ranges-', {}, [{ id: 1n, text, timestamp: 1n }], (store) => {
    for (const extra of [{}, { timestampRange: { start: 0n, end: 2n } }]) {
      const { snippet } = store.query({ text: 'harbour cafe', k: 1, snippetBytes: 22, ...extra }).hits[0];
      const bytes = Buffer.from(text);
      assert.equal(snippet.sourceByteStart, Buffer.byteLength('前 🚀 '));
      assert.equal(snippet.sourceByteEnd, snippet.sourceByteStart + Buffer.byteLength(snippet.text));
      assert.equal(bytes.subarray(snippet.sourceByteStart, snippet.sourceByteEnd).toString(), snippet.text);
      assert.ok(snippet.highlights.length >= 3);
      for (const mark of snippet.highlights) {
        assert.equal(mark.sourceByteStart, snippet.sourceByteStart + Buffer.byteLength(snippet.text.slice(0, mark.start)));
        assert.equal(mark.sourceByteEnd, snippet.sourceByteStart + Buffer.byteLength(snippet.text.slice(0, mark.end)));
        assert.equal(bytes.subarray(mark.sourceByteStart, mark.sourceByteEnd).toString(), snippet.text.slice(mark.start, mark.end));
      }
    }
  });
});
