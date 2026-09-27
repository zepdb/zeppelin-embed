# Zeppelin Embed for Node.js

`@zepdb/zeppelin-embed` provides in-process vector, text and hybrid search for
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

The engine stores all 128 bits of an ID and every API returns it unchanged,
so the whole UUID range fits. `uuidToId` reads a UUID's 32 hex digits as one
big-endian integer and `idToUuid` formats an ID back as a lowercase UUID; the
pair is exact for every 128-bit value:

```js
const { uuidToId, idToUuid } = require('@zepdb/zeppelin-embed');

const id = uuidToId('123e4567-e89b-12d3-a456-426614174000');
store.upsert([{ id, text: 'meeting notes' }]);
const hit = store.query({ text: 'meeting', k: 1 }).hits[0];
console.log(idToUuid(hit.id)); // '123e4567-e89b-12d3-a456-426614174000'
```

`uuidToId` accepts only the `8-4-4-4-12` hex form (either case) and throws a
`TypeError` for anything else; `idToUuid` throws a `RangeError` for a bigint
outside `0n..2n ** 128n - 1n`.

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

Each document has a caller-chosen `revision` (`1n` when omitted) that only
moves forward per id: a higher revision replaces the document, the same
revision is an idempotent retry that writes nothing, and a lower one throws
`ZE_ERR_STALE_REVISION`. A deleted id keeps its last revision as that floor.
`get` returns the live revision.

An upsert or delete entry can carry `expectedRevision`, which makes the whole
call a compare-and-set: a `bigint` requires a live document at exactly that
revision, and `null` requires that no live document exists (never written, or
deleted). The single writer checks every condition against the latest
committed state, atomically with the write. If any condition fails, the call
writes nothing and throws `ZE_ERR_REVISION_CONFLICT` with a `conflict` that
names the failed entry:

```js
const head = store.get([headId]).documents[0];
try {
  store.upsert([
    { id: headId, revision: head.revision + 1n, expectedRevision: head.revision },
    { id: segmentId, expectedRevision: null },
  ]);
  store.delete([{ id: draftId, expectedRevision: 3n }]);
} catch (error) {
  if (error.code !== 'ZE_ERR_REVISION_CONFLICT') throw error;
  // error.conflict: { index, id, expectedRevision, currentRevision }
}
```

Omit `vectorSpace` for a record-only namespace. Scan cursors are opaque and
must be passed back unchanged; a cursor invalidated by a write throws
`ZE_ERR_SCAN_STALE`.

A scan can also order by a declared `u64`, `i64` or `f64` attribute:

```js
const page = store.scan({
  limit: 50,
  order: { attributeId: 1, direction: 'descending' },
});
const next = store.scan({
  limit: 50,
  order: { attributeId: 1, direction: 'descending' },
  cursor: page.cursor,
});
```

Equal values break ties by ascending document id, so pages are stable.
`f64` compares numerically and `-0` equals `+0`. A document whose value is
missing or `NaN` sorts after every document with a value, in both
directions. A cursor works only with the order that issued it; any other
order, attribute or direction throws `ZE_ERR_INVALID_ARGUMENT`. Ordering by an
undeclared attribute, by attribute 0 (the timestamp), or by a `bool` or string
attribute throws `ZE_ERR_INVALID_ARGUMENT`.

A later release can add attributes without re-ingesting. Declare the new
attribute as nullable and open the namespace for writing; documents written
before it read it as null, exactly like a document written without it
(`isNull` matches them; `eq`, `in`, `range` and `exists` do not):

```js
const store = openNamespace('my-database', 'documents', {
  attributes: [
    { id: 1, name: 'category', type: 'dictionaryString' },
    { id: 2, name: 'language', type: 'dictionaryString', nullable: true },
  ],
  vectorSpace: { dimensions: 2 },
});
```

Attributes match by `id`. Every stored attribute must be declared unchanged.
Removing or changing one, adding a non-nullable one, or adding one on a
`readOnly` open throws `ZE_ERR_SCHEMA_MISMATCH` naming the attribute. Once a
release adds an attribute, earlier releases can no longer open the namespace
with their shorter declaration.

One `upsert`, `ingest` or `delete` call is one atomic batch. If the process
or the machine stops at any point during the call, the next writable open
shows every document of the batch or none of them, and every batch whose call
returned is still there. Put documents that must change together (a note head
and its body, a transcript version and its head pointer) in one call.

A batch is atomic only inside one namespace. Each namespace has its own
write-ahead log, so an `upsert` to `notes` and an `upsert` to `segments` are
two independent batches, and a crash between them can keep the first without
the second. Keep documents that must change together in one namespace, or
make the second write repairable from the first (for example, write the
dependent documents first and the pointer that makes them live last).

A read-only open never repairs: if the last write was cut mid-record, it
throws `ZE_ERR_CORRUPT` until a writable open has cut that record off.

Writes land in an active segment backed by the write-ahead log. `seal()`
turns the active segment into an immutable segment and absorbs the log
records it covers, so opening the store again reads the sealed segment
instead of replaying those writes. It then truncates the log to its
header, so `wal.ze` holds only the writes since the last seal and stays
bounded under `autoSealRows`; a crash during that step loses nothing.
Seal after a bulk load, when the
application is idle, or let the store do it with `autoSealRows`:

```js
const store = openNamespace('my-database', 'notes', { attributes: [] }, {
  autoSealRows: 2048,
  autoMerge: true,
});
store.seal(); // { generation }; a no-op when nothing is unsealed
```

With `autoSealRows`, the store seals once at open and again before the write
that follows that many written documents since the last seal. A smaller value
keeps each write cheaper, because a write copies the active segment; a larger
value makes fewer sealed segments. `autoSealRows` is disabled by default and
must be a positive safe integer. It counts documents (including revisions and
deleted IDs), not bytes or calls; a batch may exceed the threshold before the
next write seals it.

`autoMerge` defaults to `false`. With `true`, open seals and merges existing
writes, and every subsequent automatic or explicit seal runs `merge()`. Pair
both options as above so the application need not schedule maintenance. Both
require a writable store. Merging starts when at least two compatible small
scan segments fit the native batch limits below; there is no additional timer
or configurable segment-count threshold. It drops dead revisions in selected
segments, including stored text. Live retained data still consumes memory and
disk; these options do not implement retention or cap total store size.

Automatic maintenance is synchronous and adds latency to the triggering open,
seal or write. Errors propagate before the pending write; already completed
maintenance remains committed. `seal()` returns the final merge generation.
With automatic merging disabled, call `store.merge()` during application idle
time to combine small sealed segments and reduce first-query cost after reopen. This synchronous call leaves active writes and the WAL alone;
call `seal()` first to include the current writes. It publishes each replacement
before removing the inputs, in batches of at most 16 segments and 8 MiB of input
files. Decoded working memory exceeds those input bytes. Large segments and graph
segments stay separate. The result is `{ generation }`, unchanged if no batch
fits; a failure can follow already committed batches.
With an open snapshot, explicit and automatic merges still publish replacements,
but retain their input files. Close the snapshots and reopen the writable store
to reclaim those retired files. Snapshot reads and cursors keep their original
generation throughout sealing and merging.

`deleteWhere` deletes every document that matches a filter, in one mutation,
and removes their bytes from disk. It takes the same `Filter` as `scan` and
`count`, and the filter is required:

```js
const { deleted, generation } = store.deleteWhere({
  op: 'eq',
  attributeId: 1,
  values: [{ id: 1, type: 'u64', value: 42n }],
});
```

No other write can land between finding the matches and deleting them, and
readers see all of the matched documents or none of them. When the call
returns, no byte of a deleted document remains in any store file: each
affected sealed segment and the write-ahead log are rewritten without it, so
there is no separate compaction step. If the process stops during the call,
either nothing was deleted or the next writable open finishes the removal.
`delete(ids)` only hides documents; their bytes stay on disk.

`count` returns the live documents that match an optional `filter` and
`timestampRange`. With `groupBy` it also returns one count per value of a
`u64`, `i64`, `dictionaryString` or `rawString` attribute, all from one
generation:

```js
store.count({ groupBy: { attributeId: 1 } });
// { count: 5n, generation: 7n, missingCount: 1n,
//   groups: [{ value: 'home', count: 1n }, { value: 'work', count: 3n }] }
```

Groups are in ascending value order: numeric for integers, byte order for
strings. Documents whose attribute is null are not a group; they are in
`missingCount`, so the group counts plus `missingCount` equal `count`.
`groupBy.limit` (default 1024, at most 65536) bounds the number of distinct
values; more values throw `ZE_ERR_BUDGET_EXCEEDED` and no group is dropped.
Other attribute types throw `ZE_ERR_INVALID_ARGUMENT`. There is no timestamp
bucketing: to count per day, store a day number as an `i64` attribute and
group by it.

`openSnapshot()` pins an in-place read-only view of the current generation:

```js
const view = store.openSnapshot();
try {
  const first = view.scan({ limit: 100 });
  // Writes, seals and logical deletes on store do not change view or its cursors.
  const next = first.cursor ? view.scan({ limit: 100, cursor: first.cursor }) : null;
} finally {
  view.close();
}
```

The source must be writable. The view shares active data and sealed mappings;
opening it creates no files. All its reads use the pinned generation, and its
mutation methods return `ZE_ERR_ACCESS_MODE`. It remains readable after the
source closes. Close every view explicitly: while any is open, `purge()` and
`deleteWhere()` return `ZE_ERR_STORE_BUSY` before mutation. Retired segment paths
remain until the next writable open (or a later physical purge). If the source
closes first, its writer lock stays held until the views close, preventing a new
writer from reclaiming their files.

Use `openSnapshot()` on the existing namespace handle to pin it. Opening another
writable `openNamespace()` is a second writer and returns `ZE_ERR_STORE_BUSY`.
This is distinct from `ZE_ERR_BUSY`, which means concurrent FFI writer calls on
one handle. Snapshot admission uses the existing handle and waits for core
admission locks; it does not attempt either writer-lock admission path.

`snapshot(target)` writes a consistent copy of the store at one generation
into a directory, for a backup or an export, while the application keeps
writing. It runs on a worker thread and resolves to `{ generation }`; writes
made while it runs are not in the snapshot. The target must not exist or must
be an empty directory, its parent must exist, and it must not be inside the
store. A failed snapshot never creates the target. The snapshot is an ordinary
store: open it, read-only or read-write, with the same namespace spec to
restore that state.

```js
const { generation } = await store.snapshot('/Volumes/Backup/notes-2026-09-24');
const restored = openNamespace('/Volumes/Backup', 'notes-2026-09-24',
  { attributes: [] }, { readOnly: true });
```

A crash during a snapshot can leave a hidden `.<name>.snapshot-*.tmp`
directory beside the target. It is never a snapshot and can be deleted.

`verify` checks a store directory end to end without opening or changing it:
the manifest, every segment's checksums and index regions, and the
write-ahead log through the same replay recovery runs. Run it after an unclean
shutdown, before reopening, or from diagnostics. Damage comes back as
findings rather than an exception:

```js
const { verify } = require('@zepdb/zeppelin-embed');

const report = verify(path.join('my-database', 'notes'));
if (!report.ok) {
  for (const finding of report.findings) {
    // { kind: 'walRecordCorrupt', file: 'wal.ze', offset: 40n, detail: '...' }
    console.error(finding.kind, finding.file, finding.offset, finding.detail);
  }
}
```

Every finding is damage: the store will not open, or would lose data. A torn
log tail is damage too, because recovery refuses it. The package also installs
a `zeppelin-verify <store-directory>` command that prints the same report as
JSON and exits 0 when the store is clean, 1 when it found damage, and 2 when
it could not run.

`query` runs one structured query. `text` selects the lexical leg, `vector`
selects the vector leg, and both together run hybrid fusion; a request with
neither is refused.

```js
const { CancellationToken } = require('@zepdb/zeppelin-embed');

// Lexical, with the last term treated as a type-ahead prefix.
store.query({ text: 'harb', k: 10, lastAsPrefix: true });

// Hybrid, with an explicit fusion weight.
const result = store.query({
  text: 'harbour lights',
  vector: new Float32Array([0.9, 0.1]),
  k: 10,
  alpha: 0.75,
});
console.log(result.mode, result.fusion.effectiveAlpha, result.hits);
```

Each hit carries `id`, `score`, and the per-leg `lexicalBm25` and
`vectorSquaredL2` when that leg ran; `revision` is absent on a fused hit,
which carries identity only. The result carries the queried `generation`, the
`mode` that ran, the `approximate`, `exactRescore`, and `budgetExhausted`
flags, and a `fusion` report when both legs ran.

Set `snippetBytes` to also get an excerpt of each hit's stored text with the
matched terms marked. The engine marks them with the same analyzer and the
same scored terms the query used, so stemmed, accent-folded, and
prefix-expanded forms are marked exactly as they matched. Without
`snippetBytes` the query reads no extra text.

```js
const { hits } = store.query({ text: 'harbour cafe', k: 10, snippetBytes: 120 });
for (const { snippet } of hits) {
  if (snippet === undefined) continue; // a hybrid hit the text did not match
  const marked = snippet.highlights.map(({ start, end }) => snippet.text.slice(start, end));
  const prefix = snippet.truncatedStart ? '…' : '';
  const suffix = snippet.truncatedEnd ? '…' : '';
  console.log(prefix + snippet.text + suffix, marked);
}
```

`highlights` are ascending, non-overlapping `{ start, end }` ranges in UTF-16
code units into `snippet.text`, so `String.prototype.slice` takes them
directly. The excerpt starts at a matched word and covers `snippetBytes` bytes
of UTF-8, plus at most 3 to finish a character; the window with the most
distinct matches wins. The engine adds no ellipsis; `truncatedStart` and
`truncatedEnd` say where the excerpt was cut. A document has one text field, so
a hit has at most one `snippet`. It is absent only on a hybrid hit whose text
contains no query term. The query operators are terms and a trailing prefix;
a quoted phrase is not matched as a phrase, so each of its terms is marked.

A query stops early on either a `deadlineNs` or a `cancelToken`, never both.
A `CancellationToken` owns an engine handle, so close it:

```js
const token = new CancellationToken();
try {
  const hits = store.query({ text: 'harbour', k: 10, cancelToken: token });
  // token.cancel() from elsewhere asks any query holding it to stop.
} finally {
  token.close();
}
```

A lexical query works on a record-only namespace. A hybrid query there throws
`ZE_ERR_NO_VECTOR_SPACE`.

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

## Electron

The Node package workflow qualifies Electron 44.4.1 on macOS arm64 and x64
using a minimal electron-builder app with an ASAR archive. It runs the addon
in the main process, in a utility process, and in two utility processes using
different stores. Node tests also exercise worker threads, garbage collection,
and process exit with an unclosed store.

Declare `@zepdb/zeppelin-embed` as a production dependency and include this in
your electron-builder configuration:

```json
{
  "asar": true,
  "asarUnpack": ["node_modules/@zepdb/zeppelin-embed/prebuilds/**/*.node"],
  "npmRebuild": false
}
```

The unpack pattern is relative to the app directory. It keeps the native files
in `app.asar.unpacked`; Electron redirects their ASAR paths automatically, so
keep using `require('@zepdb/zeppelin-embed')`. The published prebuilds need no
rebuild for macOS Electron. `npmRebuild: false` is appropriate for this minimal
app; applications with other native dependencies must handle those dependencies'
rebuild requirements separately. Windows requires the shipped Electron-major
variant described above.

Call `utilityProcess.fork` after `app.whenReady()`. In the utility entry point:

```js
const { openNamespace } = require('@zepdb/zeppelin-embed');
const store = openNamespace('/absolute/writable/data/path', 'notes', {});
try {
  store.upsert([{ id: 1n, text: 'harbour lights' }]);
  console.log(store.query({ text: 'harbour', k: 1 }));
} finally {
  store.close();
}
```

Keep stores outside the signed app bundle. Each store has one writer; different
processes must use different stores. Loading the addon twice is tested by
removing both its JavaScript and native entries from the CommonJS cache and
asserting two native initializations, independent live stores, and persisted
results after close and reopen. Ordinary repeated `require` also works.

The macOS addon is built with a macOS 11.0 deployment target. The Electron
44.4.1 app bundle declares macOS 13.0 as its minimum; the host runtime can
require a newer OS than the addon. CI executes on macos-14, not every older OS.
The addon performs no runtime network requests; installation and CI may download
packages and Electron.

### Signing scope

CI uses only ad-hoc signing (`mac.identity: "-"`), with
`mac.hardenedRuntime: true` and `mac.notarize: false`. It verifies the app,
utility helper, and unpacked addon with `codesign --verify --strict`, checks
that their signatures are ad-hoc and carry the runtime flag, and checks the
whole bundle with `--deep --strict` before launching it.

The fixture grants `com.apple.security.cs.allow-jit` for Electron/V8 and
`com.apple.security.cs.disable-library-validation` because ad-hoc code has no
shared Team ID for library validation. It grants no unsigned-executable-memory
exception. These are host signing settings, not an addon JIT requirement.
Hardened runtime remains enabled with those explicit exceptions.

This proves packaged loading and signature integrity under the tested ad-hoc
configuration. It does not authenticate a publisher, prove Gatekeeper trust,
provide notarization or stapling, or prove loading with library validation
enabled. A future Developer ID qualification must sign all nested code with
one real identity, remove the ad-hoc library-validation exception and retest,
then separately notarize, staple, and assess distribution. No certificate or
notarization credentials are required for this CI fixture.

Zeppelin Embed is licensed under GPL-3.0-only.

### Read-only diagnostics shell

The package installs `zeppelin-shell` (Node 18+ and the packaged native addon).
Run it against an extracted, quiescent **copy** of a store. Every open is
read-only: the shell has no mutation commands or writable mode, never repairs a
WAL, and does not create missing stores. A truncated WAL may prevent reads;
`verify` works without opening the store and reports the damage.

```sh
zeppelin-shell /diagnostics/root namespaces
zeppelin-shell /diagnostics/root/docs schema
zeppelin-shell /diagnostics/root/docs get 18446744073709551617
zeppelin-shell /diagnostics/root/docs scan '{"order":{"attributeId":1,"direction":"descending"},"limit":10,"filter":{"op":"exists","attributeId":1}}'
zeppelin-shell /diagnostics/root/docs count '{"filter":{"op":"eq","attributeId":1,"values":[{"id":1,"type":"u64","value":{"$bigint":"42"}}]}}'
zeppelin-shell /diagnostics/root/docs query '{"text":"hello world","k":10}'
zeppelin-shell /diagnostics/root/docs dump '{"limit":100}' > documents.jsonl
zeppelin-shell /diagnostics/root/docs verify
```

From this checkout, substitute `node bindings/node/bin/zeppelin-shell.js` for
`zeppelin-shell`. `--help` lists the commands. `namespaces` takes a namespace
root; all other commands take a store directory. Namespace listings include
attribute ids, names, types and nullability (the built-in timestamp is omitted).

Request objects use the Node API's `ScanRequest`, `CountRequest` and
`QueryRequest` shapes. Represent bigint inputs as `{"$bigint":"123"}` to avoid
JSON number rounding. `get` takes an unsigned decimal 128-bit id. `scan` returns
one page; `dump` follows all pages, includes every stored field, and treats
`limit` as page size. Its optional filter and order apply to the whole dump.
JSON output encodes bigint values as decimal strings, vectors and metadata as
arrays, and non-finite floats as `"NaN"`, `"Infinity"`, or `"-Infinity"`.
Dump output is for inspection, not an import format. Errors go to stderr;
exit codes are 0 for success, 1 for verify findings, and 2 for usage/open errors.

The same inspection capabilities are available programmatically:

```js
const { openInspection } = require('@zepdb/zeppelin-embed');
const store = openInspection('/diagnostics/root/docs');
try {
  console.log(store.schema());
  console.log(store.count());
} finally {
  store.close();
}
```

`openInspection` reads the persisted identity and schema without requiring the
original application's namespace spec. It retains tokenizer compatibility
checks and rejects writes. Ordinary `Store` and `openNamespace` epoch/schema
validation is unchanged. This shell inspects document stores, not graph stores.
## Graph documents and Cypher — 0.5.0 MVP

**Disk reclamation.** Writable graph stores default to `autoReclaim: true`
with `reclaimAfterBytes: 67108864` (64 MiB). Set a safe integer threshold of
at least 1 MiB to reclaim more often, or set `autoReclaim: false` and call
`maintain()` / `maintainAsync()` explicitly. Each call returns a scalar report;
loop until `cycleComplete` to finish a cycle. Automatic work runs before the
next write after the threshold, taking up to eight cycles of at most four
bounded steps each. It can add write latency and propagates maintenance errors
before staging that write. The options are refused on read-only opens.


`GraphStore` uses the existing graph store format. A document is a node, so a
single `apply` commits document nodes and relationships atomically. A legacy
`Store` directory cannot be opened as a graph store or participate in its writes.
This labelled graph MVP ships on darwin-arm64, darwin-x64 and win32-x64
(node-napi8 and electron-44). The macOS deployment target stays 11;
`GraphStore.isSupported()` returns false below macOS 14, and `open` throws
`ZeppelinError` with `ZE_ERR_UNSUPPORTED`. Full qualification remains ZE-78.
A Cypher statement creates about 121 nodes under default budgets; use `apply`
for bulk writes. Graph search inside Cypher is not included (ZE-58).

```js
const { GraphStore } = require('@zepdb/zeppelin-embed');
const graph = GraphStore.open('/path/to/new-graph');
try {
  const result = graph.apply([
    { kind: 'node', operation: 'create', namespace: 'notes', key: 'note-1',
      revision: 1n, labels: ['Note'], text: 'Meeting notes',
      properties: { title: 'Planning', done: false } },
    { kind: 'node', operation: 'create', namespace: 'folders', key: 'work',
      revision: 1n, labels: ['Folder'], properties: { name: 'Work' } },
    { kind: 'relationship', operation: 'create', namespace: 'filing', key: 'note-1/work',
      revision: 1n, type: 'IN_FOLDER', source: { local: 0 }, target: { local: 1 } },
  ]);
  console.log(result.disposition, result.generation, result.receipts);
  console.log(graph.cypher(
    'MATCH (n:Note)-[:IN_FOLDER]->(f:Folder) WHERE f.name = $name RETURN n, ze.stored_text(n)',
    { name: 'Work' }, { maxRows: 4096 },
  ).rows);
} finally {
  graph.close();
}
// Reopen explicitly: GraphStore.open(path, { mode: 'readWrite' }) or 'readOnly'.
```

Mutations are keyed by `(namespace, key)` with positive unsigned 64-bit bigint
revisions. Exact retries report `Replayed`. `put` replaces the full image and
requires `expectedId`; `delete` also requires `expectedId` (node deletion accepts
`detach: true`); `recreate` requires `expectedDeletionRevision`. Endpoints are
node IDs (`bigint`) or `{ local: index }` references into the same batch. At most
16,384 items are accepted. Omitted image fields are empty/absent, not patches.

Query results contain `columns`, `rows`, `receipts`, `disposition`,
`admittedGeneration`, `changedGeneration`, and `generation` (changed when known,
otherwise admitted). Values are null, boolean, I64 bigint, F64 number, string,
node/relationship objects with a `kind` discriminator, or lists. Properties use
scalar values or homogeneous scalar lists. Parameters are scalar only. Pass a
bigint for an integer; ordinary JavaScript numbers are F64. IDs are unsigned
128-bit bigints. Source text is distinct from properties; read it with
`ze.stored_text(n)`.

`maxRows` accepts integers 0..65,536; omitted or zero selects 1,024. Exceeding the
cap throws `ZE_ERR_BUDGET_EXCEEDED` without truncation. Raising it permits larger results,
subject to the engine's separate memory/work limits. Errors preserve settlement
dispositions, including `NotCommitted`, `Committed`, `Replayed`, and
`Indeterminate`; do not blindly retry a write because it threw. Invalid JS inputs
are rejected before calling the engine.

The [documented Cypher profile](https://github.com/zepdb/zeppelin-embed/blob/main/crates/zeppelin-embed-cypher/README.md)
defines supported statements and functions. This binding does not add syntax,
graph search, vector inputs, or list parameters. The example uses synchronous
calls; use the async API with `AbortSignal` for potentially long operations.

The normal Node suite includes `test/bounded-soak.test.mjs`: eight compressed
meeting hours (one transcript per second and one note edit per five seconds),
then eight hours of note edits with a fixed live corpus. It samples process RSS,
WAL and total store bytes hourly, without explicit maintenance or forced GC.
The first half of the fixed-corpus phase warms allocator and merge state;
the second half must grow by less than 64 MiB from that warmed baseline.
Run `ZE_LONG_SOAK=1 node --test test/bounded-soak.test.mjs` for 48 hours per phase
(up to 30 minutes instead of the normal three-minute deadline). The workload
uses `commitTier: 'none'` to measure engine behavior without per-write fsync;
it does not qualify power-loss durability or weeks of real-time use.
