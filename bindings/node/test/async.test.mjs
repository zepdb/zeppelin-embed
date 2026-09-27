import assert from 'node:assert/strict';
import { mkdtempSync, rmSync } from 'node:fs';
import { tmpdir } from 'node:os';
import { join } from 'node:path';
import { createRequire } from 'node:module';
import test from 'node:test';
const { Store, GraphStore, openNamespace, openNamespaceAsync, ZeppelinError } = createRequire(import.meta.url)('..');
const cancelled = e => e instanceof ZeppelinError && e.code === 'ZE_ERR_CANCELLED';
async function fixture(fn) {
  const root = mkdtempSync(join(tmpdir(), 'ze249-'));
  const s = openNamespace(root, 'notes', {});
  try { await fn(s, root); } finally { try { s.close(); } catch {} rmSync(root, { recursive: true, force: true }); }
}
test('async store operations equal sync results', () => fixture(async (s, root) => {
  const docs = [{ id: 1n, text: 'hello world' }, { id: 2n, text: 'hello again' }];
  assert.equal(typeof (await s.upsertAsync(docs)).generation, 'bigint');
  assert.deepEqual(await s.queryAsync({ text: 'hello' }), s.query({ text: 'hello' }));
  assert.deepEqual(await s.queryAsync({ text: 'hello', snippetBytes: 32 }), s.query({ text: 'hello', snippetBytes: 32 }));
  const page = await s.scanAsync({ limit: 1 });
  assert.deepEqual(page, s.scan({ limit: 1 }));
  assert.deepEqual(await s.scanAsync({ cursor: page.cursor }), s.scan({ cursor: page.cursor }));
  assert.deepEqual(await s.scanAsync(), s.scan());
  await s.sealAsync();
  assert.deepEqual(await s.mergeAsync(), s.merge());
  assert.deepEqual(await s.maintainAsync(), s.merge());
  const token = await s.purgeAsync([1n], { wait: false });
  assert.equal(typeof (await s.awaitPurgeAsync(token.tokenId)).generation, 'bigint');
  assert.equal((await s.purgeAsync([999n])).isNoOp, true);
  await s.backupAsync(join(root, 'backup'));
  const backup = await openNamespaceAsync(root, 'backup', {}, { readOnly: true });
  try { assert.deepEqual(await backup.scanAsync(), s.scan()); } finally { backup.close(); }
}));
test('async calls remain responsive and close safely', () => fixture(async s => {
  await s.upsertAsync([{ id: 1n, text: 'hello' }]);
  let ticked = false;
  const tick = new Promise(resolve => setImmediate(() => { ticked = true; resolve(); }));
  const pending = Array.from({ length: 20 }, () => s.queryAsync({ text: 'hello' }));
  s.close();
  const outcomes = await Promise.allSettled(pending);
  await tick;
  assert.equal(ticked, true);
  for (const r of outcomes) if (r.status === 'rejected') assert.equal(r.reason.code, 'ZE_ERR_CLOSED');
  await assert.rejects(s.scanAsync(), e => e instanceof ZeppelinError && e.code === 'ZE_ERR_CLOSED');
}));
test('query and scan AbortSignal reject promptly without partial results', () => fixture(async s => {
  await s.upsertAsync(Array.from({ length: 10000 }, (_, i) => ({ id: BigInt(i), text: 'hello world' })));
  for (const method of ['queryAsync', 'scanAsync']) {
    const c = new AbortController(); c.abort();
    await assert.rejects(s[method]({ text: 'hello', signal: c.signal }), cancelled);
    const running = new AbortController();
    const start = performance.now();
    const p = s[method]({ text: 'hello', limit: 10000, signal: running.signal });
    running.abort();
    await assert.rejects(p, cancelled);
    assert.ok(performance.now() - start < 2000, 'cancellation settles within 2s');
  }
}));
test('graph async parity cancellation and close', { skip: !GraphStore.isSupported() }, async () => {
  const root = mkdtempSync(join(tmpdir(), 'ze249-graph-'));
  const s = GraphStore.open(join(root, 'graph'));
  try {
    await s.applyAsync([{ kind: 'node', operation: 'create', namespace: 'docs', key: 'a', revision: 1n }]);
    assert.deepEqual(await s.cypherAsync('MATCH (n) RETURN n'), s.cypher('MATCH (n) RETURN n'));
    const c = new AbortController(); c.abort();
    await assert.rejects(s.cypherAsync('RETURN 1', {}, { signal: c.signal }), cancelled);
    const p = s.cypherAsync('RETURN 1'); s.close();
    await p.catch(e => assert.equal(e.code, 'ZE_ERR_CLOSED'));
    await assert.rejects(s.cypherAsync('RETURN 1'), e => e.code === 'ZE_ERR_CLOSED');
    const writer = GraphStore.open(join(root, 'graph'), { mode: 'readWrite' });
    const write = writer.applyAsync([{ kind: 'node', operation: 'create', namespace: 'docs', key: 'b', revision: 1n }]);
    writer.close();
    await write.catch(e => assert.equal(e.code, 'ZE_ERR_CLOSED'));
  } finally { s.close(); rmSync(root, { recursive: true, force: true }); }
});

test('async open and mutation policies preserve ownership and errors', async () => {
  const root = mkdtempSync(join(tmpdir(), 'ze249-open-'));
  let s;
  try {
    s = await Store.openAsync(join(root, 'raw'));
    assert.deepEqual(await s.scanAsync(), s.scan());
    s.close();
    s = await openNamespaceAsync(root, 'notes', {}, { autoSealRows: 1, autoMerge: true });
    const input = [{ id: 1n, text: 'original' }];
    const first = s.upsertAsync(input); input[0].text = 'changed';
    const second = s.upsertAsync([{ id: 2n, text: 'second' }]);
    await Promise.all([first, second]);
    assert.equal(s.get([1n], { text: true }).documents[0].text, 'original');
    await assert.rejects(s.upsertAsync([{ id: 1n, revision: 2n, expectedRevision: 99n, text: 'bad' }]), e => {
      assert.ok(e instanceof ZeppelinError);
      assert.equal(e.code, 'ZE_ERR_REVISION_CONFLICT');
      assert.equal(e.conflict.id, 1n);
      return true;
    });
    await s.upsertAsync([{ id: 3n, text: 'after failure' }]);
    await s.snapshotAsync(join(root, 'copy'));
    s.close();
    s = await openNamespaceAsync(root, 'notes', {}, { autoSealRows: 1, autoMerge: true });
    assert.equal(s.count().count, 3n);
    await assert.rejects(Store.openAsync(join(root, 'invalid'), { autoMerge: true, readOnly: true }), /readOnly/);
    const pending = s.upsertAsync([{ id: 4n, text: 'queued' }]);
    s.close();
    await assert.rejects(pending, e => e.code === 'ZE_ERR_CLOSED');
  } finally { try { s?.close(); } catch {} rmSync(root, { recursive: true, force: true }); }
});

test('pending query scan and cypher cancel from a microtask within 2s', { timeout: 15000 }, async () => {
  const root = mkdtempSync(join(tmpdir(), 'ze249-running-'));
  const dimensions = 512;
  const s = openNamespace(root, 'vectors', { vectorSpace: { dimensions } });
  let graph;
  let closed = false;
  async function abortWhilePending(run) {
    const controller = new AbortController();
    let settled = false;
    const pending = run(controller.signal);
    const outcome = pending.then(value => { settled = true; return { value }; }, error => { settled = true; return { error }; });
    // A 1 ms timer can run after native completion but before JS receives it,
    // especially under Rosetta/load. Yield to a microtask instead: abort after
    // submission, before waiting for a timer turn. This proves pending-call
    // cancellation (queued or executing), not that the worker has started.
    // Keep the 2 s promptness bound measured from the actual abort.
    await new Promise(resolve => queueMicrotask(resolve));
    assert.equal(settled, false, 'call still pending before abort');
    const start = performance.now();
    controller.abort();
    const result = await outcome;
    assert.ok(cancelled(result.error), String(result.error));
    assert.ok(performance.now() - start < 2000, 'engine stops within 2s of abort');
  }
  try {
    const vector = new Float32Array(dimensions).fill(0.25);
    await s.upsertAsync(Array.from({ length: 50000 }, (_, i) => ({ id: BigInt(i), vector, timestamp: BigInt(50000 - i) })));
    await abortWhilePending(signal => s.queryAsync({ vector, k: 50000, tier: 'exact', signal }));
    await abortWhilePending(signal => s.scanAsync({ limit: 50000, order: 'timestampAscending', signal }));
    if (GraphStore.isSupported()) {
      graph = GraphStore.open(join(root, 'graph'));
      await graph.applyAsync(Array.from({ length: 33 }, (_, i) => ({ kind: 'node', operation: 'create', namespace: 'n', key: String(i), revision: 1n })));
      await abortWhilePending(signal => graph.cypherAsync('MATCH (a), (b), (c), (d) RETURN count(a)', {}, { signal }));
    }
    const pending = s.queryAsync({ vector, k: 50000, tier: 'exact' });
    const outcome = pending.then(() => null, error => error);
    await new Promise(resolve => setTimeout(resolve, 1));
    s.close(); closed = true;
    const error = await outcome;
    if (error) assert.equal(error.code, 'ZE_ERR_CLOSED');
  } finally { if (!closed) s.close(); graph?.close(); rmSync(root, { recursive: true, force: true }); }
});

test('async upsert preserves every field through the shared namespace-batch parser', async () => {
  const root = mkdtempSync(join(tmpdir(), 'ze249-parser-'));
  const spec = {
    vectorSpace: { dimensions: 2 },
    attributes: [{ id: 1, name: 'rank', type: 'u64' }, { id: 2, name: 'label', type: 'dictionaryString' }],
  };
  const document = {
    id: (3n << 64n) + 5n, revision: 7n, timestamp: -11n,
    vector: new Float32Array([1.25, -2.5]), text: 'stored text', metadata: new Uint8Array([7, 8, 9]),
    attributes: [{ id: 1, type: 'u64', value: 42n }, { id: 2, type: 'string', value: 'alpha' }],
  };
  const sync = openNamespace(root, 'sync', spec);
  let async;
  try {
    async = await openNamespaceAsync(root, 'async', spec);
    const expected = sync.upsert([{ ...document, expectedRevision: null }]);
    assert.deepEqual(await async.upsertAsync([{ ...document, expectedRevision: null }]), expected);
    assert.deepEqual(async.get([document.id]), sync.get([document.id]));
    assert.deepEqual(async.get([document.id]).documents[0], document);
    await assert.rejects(async.upsertAsync([{ ...document, expectedRevision: null }]), e => e.code === 'ZE_ERR_REVISION_CONFLICT');
  } finally { sync.close(); async?.close(); rmSync(root, { recursive: true, force: true }); }
});
