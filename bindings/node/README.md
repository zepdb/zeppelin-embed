# Zeppelin Embed for Node.js

`@zepdb/zeppelin-embed` provides in-process vector search for
macOS on Apple silicon and Intel, and for Windows x64. The package uses stable
Node-API and includes its native engine, so it does not need a separate dynamic
library at runtime.

```bash
npm install @zepdb/zeppelin-embed
```

```js
const { Store } = require('@zepdb/zeppelin-embed');

const store = new Store('my-index');
store.ingest(
  [{ id: 1n, vector: new Float32Array([0.9, 0.1]) }],
  2,
);

console.log(store.search(new Float32Array([0.8, 0.2]), 1));
store.close();
```

Applications supply document and query vectors. Document IDs are unsigned
128-bit `bigint` values. Close each store when it is no longer needed; the
native finalizer also closes an open handle if the JavaScript object is
collected.

Namespaces add typed records without changing the existing vector API:

```js
const { openNamespace, listNamespaces } = require('@zepdb/zeppelin-embed');

const store = openNamespace('my-database', 'documents', {
  attributes: [{ id: 1, name: 'category', type: 'dictionaryString' }],
  vectorSpace: { dimensions: 2 },
});
store.upsert([{
  id: 1n,
  vector: new Float32Array([0.9, 0.1]),
  text: 'stored text',
  attributes: [{ id: 1, type: 'string', value: 'example' }],
}]);
console.log(store.get([1n]));
console.log(store.scan({ limit: 100 }));
console.log(listNamespaces('my-database'));
store.close();
```

Omit `vectorSpace` for a record-only namespace. Scan cursors are opaque and
must be passed back unchanged; a cursor invalidated by a write throws
`ZE_ERR_SCAN_STALE`.

The package supports Node.js 18 or newer on macOS arm64 and macOS x64, and
Node.js with Node-API 8 or later on Windows x64. Unsupported platforms fail
during installation and report a clear error if the package is loaded directly.

macOS ships one binary per architecture and no per-runtime variant. The addon
is a bundle built with `-undefined dynamic_lookup`, so its Node-API symbols
resolve from whichever host process loads it and the same file works in Node
and in Electron.

On Windows the loader picks the binary from `process.platform`,
`process.arch`, and `process.versions.electron`, with no try-each fallback.
Under Electron it accepts only the majors this package ships a binary for,
currently 44; any other runtime is refused by name with
`UnsupportedRuntimeError`. The Windows addon links the Visual C++ runtime, so
the machine needs the Visual C++ redistributable.

Zeppelin Embed is licensed under GPL-3.0-only.
