import assert from 'node:assert/strict';
import test from 'node:test';
import { createRequire } from 'node:module';
import { mkdtempSync, rmSync } from 'node:fs';
import { tmpdir } from 'node:os';
import { join } from 'node:path';
import { fileURLToPath } from 'node:url';
import { execFileSync } from 'node:child_process';
const { TextStore } = createRequire(import.meta.url)('..');
const bundle = fileURLToPath(new URL('../../../crates/zeppelin-embed-text/tests/fixtures/ze115/gte.zem', import.meta.url));

test('TextStore exposes the text API', () => {
  assert.equal(typeof TextStore, 'function');
  for (const name of ['open', 'openAsync', 'openModel']) assert.equal(typeof TextStore[name], 'function');
  for (const name of ['ingest', 'ingestAsync', 'query', 'queryAsync', 'delete', 'maintain', 'health', 'observedComputeUnits', 'observedComputeUnitsAsync', 'close']) {
    assert.equal(typeof TextStore.prototype[name], 'function', name);
  }
});

test('text dense lexical hybrid ingest and shorter revision', {skip: process.platform !== 'darwin' || process.arch !== 'arm64'}, async () => {
  const root = mkdtempSync(join(tmpdir(), 'ze510-'));
  const store = await TextStore.openAsync(join(root, 'store'), bundle);
  try {
    const report = await store.ingestAsync([{id: 1n, revision: 1n, text: 'bronze zeppelin bronze zeppelin bronze zeppelin bronze zeppelin'}], {chunkMax: 4, chunkOverlap: 0});
    assert.ok(report.chunks > 1n);
    for (const legs of ['dense', 'lexical', 'hybrid']) {
      const result = await store.queryAsync({text: 'bronze', k: 5, legs});
      assert.ok(result.hits.length > 0, legs);
      assert.equal(result.hits[0].id, 1n);
      assert.equal(result.hits[0].revision, 1n);
    }
    store.ingest([{id: 1n, revision: 2n, text: 'bronze'}], {chunkMax: 4, chunkOverlap: 0});
    const result = store.query({text: 'bronze', k: 10, legs: 'lexical'});
    assert.equal(result.hits.length, 1);
    assert.equal(result.hits[0].revision, 2n);
    assert.equal(result.hits[0].chunk, 0);
  } finally { store.close(); rmSync(root, {recursive: true, force: true}); }
});

import { readFileSync } from 'node:fs';
import { createServer } from 'node:http';
import { createHash } from 'node:crypto';
const supported = process.platform === 'darwin' && process.arch === 'arm64';
const id = 1n;
async function fixture(run) {
  const root = mkdtempSync(join(tmpdir(), 'ze510-text-'));
  let store;
  try { store = TextStore.open(join(root, 'store'), bundle); await run(store, root); }
  finally {
    try { store?.close(); } catch (error) { if (error.code !== 'ZE_ERR_CLOSED') throw error; }
    rmSync(root, {recursive: true, force: true});
  }
}

test('text observed compute units loads the tiny CoreML plan (ZE-521)', {skip: !supported}, async t => {
  const [major, minor] = execFileSync('sw_vers', ['-productVersion'], {encoding: 'utf8'}).trim().split('.').map(Number);
  if (major < 14 || (major === 14 && minor < 4)) {
    t.skip('MLComputePlan requires macOS 14.4 or later');
    return;
  }
  const root = mkdtempSync(join(tmpdir(), 'ze521-compute-'));
  const previous = process.env.ZE_QUERY_COREML;
  try {
    const model = fileURLToPath(new URL('../../../crates/zeppelin-embed-text/tests/fixtures/ze115/tiny.mlmodel', import.meta.url));
    execFileSync('xcrun', ['coremlcompiler', 'compile', model, root]);
    process.env.ZE_QUERY_COREML = join(root, 'tiny.mlmodelc');
    const computeBundle = fileURLToPath(new URL('../../../crates/zeppelin-embed-ffi/tests/fixtures/compute_units.zem', import.meta.url));
    // Each variant must load the plan on a fresh store.
    for (const method of ['observedComputeUnits', 'observedComputeUnitsAsync']) {
      const store = TextStore.open(join(root, method), computeBundle);
      try {
        const counts = await store[method]();
        assert.notEqual(counts, null, method);
        assert.deepEqual(Object.keys(counts).sort(), ['cpuOps', 'gpuOps', 'neuralEngineOps', 'otherOps']);
        assert.equal(counts.cpuOps + counts.gpuOps + counts.neuralEngineOps + counts.otherOps, 1n);
      } finally { store.close(); }
    }
  } finally {
    if (previous === undefined) delete process.env.ZE_QUERY_COREML; else process.env.ZE_QUERY_COREML = previous;
    rmSync(root, {recursive: true, force: true});
  }
});

test('text observed compute units is absent without CoreML (ZE-521)', {skip: !supported}, async () => {
  const previous = process.env.ZE_QUERY_COREML;
  process.env.ZE_QUERY_COREML = 'off';
  try {
    await fixture(async store => {
      assert.equal(store.observedComputeUnits(), null);
      assert.equal(await store.observedComputeUnitsAsync(), null);
      store.close();
      assert.throws(() => store.observedComputeUnits(), error => error.code === 'ZE_ERR_CLOSED');
      await assert.rejects(store.observedComputeUnitsAsync(), error => error.code === 'ZE_ERR_CLOSED');
    });
  } finally {
    if (previous === undefined) delete process.env.ZE_QUERY_COREML; else process.env.ZE_QUERY_COREML = previous;
  }
});

test('lexical and hybrid return zero hits on fresh and deleted stores (ZE-518)', {skip: !supported}, () => fixture(async store => {
  for (const legs of ['lexical', 'hybrid']) {
    assert.deepEqual(store.query({text: 'bronze', k: 5, legs}).hits, []);
    assert.deepEqual((await store.queryAsync({text: 'bronze', k: 5, legs})).hits, []);
  }
  store.ingest([{id, revision: 1n, text: 'bronze zeppelin'}]);
  const removed = await store.deleteAsync([id]);
  assert.equal(typeof removed.generation, 'bigint');
  for (const legs of ['lexical', 'hybrid']) {
    assert.deepEqual(store.query({text: 'bronze', k: 5, legs}).hits, []);
    assert.deepEqual((await store.queryAsync({text: 'bronze', k: 5, legs})).hits, []);
  }
}));

test('text health maintenance cancellation and worker lifetime', {skip: !supported}, () => fixture(async store => {
  assert.equal(typeof store.health().generation, 'bigint');
  assert.deepEqual(await store.healthAsync(), store.health());
  store.ingest([{id, revision: 1n, text: 'bronze zeppelin'}]);
  const maintained = await store.maintainAsync({wallTimeNs: 100000000n, bytes: 67108864n});
  assert.ok([0, 1].includes(maintained.status));
  assert.equal(typeof maintained.graphsBuilt, 'bigint');
  assert.ok([0, 1].includes(store.maintain({wallTimeNs: 100000000n, bytes: 67108864n}).status));
  assert.equal(store.health().unresolvedFaultCount, 0n);
  const c = new AbortController(); c.abort();
  assert.throws(() => store.ingest([{id, revision: 2n, text: 'bronze'}], {signal: c.signal}), error => error.name === 'ZeppelinError' && error.code === 'ZE_ERR_INVALID_ARGUMENT' && error.errorCode === 1);
  const cancelled = error => error.code === 'ZE_ERR_CANCELLED';
  await assert.rejects(store.queryAsync({text: 'bronze', legs: 'lexical', signal: c.signal}), cancelled);
  assert.throws(() => store.query({text: 'bronze', legs: 'lexical', signal: c.signal}), cancelled);
  await assert.rejects(store.ingestAsync([{id, revision: 2n, text: 'bronze'}], {signal: c.signal}), cancelled);
  await assert.rejects(store.queryAsync({text: 'bronze', deadlineNs: 1n}), error => error.code === 'ZE_ERR_TIMEOUT');
  const pending = store.queryAsync({text: 'bronze', legs: 'lexical'});
  store.close();
  await pending.catch(error => assert.equal(error.code, 'ZE_ERR_CLOSED'));
  await assert.rejects(store.queryAsync({text: 'bronze'}), error => error.code === 'ZE_ERR_CLOSED');

}));

test('openModel downloads pinned local fixture with progress and opens offline cache', {skip: !supported}, async () => {
  const root = mkdtempSync(join(tmpdir(), 'ze510-model-'));
  const bytes = readFileSync(bundle);
  const sha = bytes => createHash('sha256').update(bytes).digest('hex');
  const manifest = Buffer.from(JSON.stringify({format: 1, name: 'tiny', bundle: 'tiny.zem', query_coreml: null,
    files: [{path: 'tiny.zem', size: bytes.length, sha256: sha(bytes)}]}));
  const requests = [];
  const server = createServer((request, response) => {
    requests.push(request.url);
    const body = request.url.endsWith('/zeppelin-model.json') ? manifest : bytes;
    response.writeHead(200, {'content-length': body.length}); response.end(body);
  });
  const previous = [process.env.ZE_MODEL_ENDPOINT, process.env.ZE_MODEL_ALLOW_HTTP];
  let store;
  try {
    await new Promise((resolve, reject) => { server.once('error', reject); server.listen(0, '127.0.0.1', resolve); });
    process.env.ZE_MODEL_ENDPOINT = `http://127.0.0.1:${server.address().port}`;
    process.env.ZE_MODEL_ALLOW_HTTP = '1';
    const options = {path: join(root, 'store'), repo: 'fixture/tiny', revision: '0123456789abcdef0123456789abcdef01234567',
      manifestSha256: sha(manifest), cacheDir: join(root, 'cache')};
    const events = [];
    store = await TextStore.openModel({...options, onProgress: event => events.push(event)});
    await new Promise(resolve => setImmediate(resolve));
    assert.equal(requests.length, 2, 'local JS HTTP server serviced the native download without blocking');
    assert.deepEqual(events[0], {done: 0n, total: 0n});
    assert.deepEqual(events.at(-1), {done: BigInt(bytes.length), total: BigInt(bytes.length)});
    await store.ingestAsync([{id, revision: 1n, text: 'bronze zeppelin'}]);
    assert.equal((await store.queryAsync({text: 'bronze', legs: 'lexical'})).hits[0].id, id);
    store.close(); store = undefined;
    store = await TextStore.openModel({...options, offline: true});
    assert.equal(requests.length, 2);
    assert.equal(store.query({text: 'bronze', legs: 'lexical'}).hits[0].id, id);
    await assert.rejects(TextStore.openModel({...options, path: join(root, 'miss'), cacheDir: join(root, 'missing'), offline: true}),
      error => error.code === 'ZE_ERR_NOT_FOUND' && /offline/i.test(error.message));
  } finally {
    store?.close(); await new Promise(resolve => server.close(resolve));
    for (const [i, key] of ['ZE_MODEL_ENDPOINT', 'ZE_MODEL_ALLOW_HTTP'].entries()) {
      if (previous[i] === undefined) delete process.env[key]; else process.env[key] = previous[i];
    }
    rmSync(root, {recursive: true, force: true});
  }
});

test('text-free prebuild reports a typed platform error', {skip: supported}, async () => {
  const unavailable = error => error.name === 'ZeppelinError' && error.code === 'ZE_ERR_UNSUPPORTED' && /text API is available on macOS arm64 only/.test(error.message);
  assert.throws(() => TextStore.open('unused', 'unused'), unavailable);
  await assert.rejects(TextStore.openAsync('unused', 'unused'), unavailable);
  await assert.rejects(TextStore.openModel({}), unavailable);
});

test('openModel offline miss is a typed not-found error without loading MLX', {skip: !supported}, async () => {
  const root = mkdtempSync(join(tmpdir(), 'ze510-offline-'));
  try {
    await assert.rejects(TextStore.openModel({path: join(root, 'store'), repo: 'fixture/tiny',
      revision: '0123456789abcdef0123456789abcdef01234567', manifestSha256: '00'.repeat(32),
      cacheDir: join(root, 'empty-cache'), offline: true}),
    error => error.name === 'ZeppelinError' && error.code === 'ZE_ERR_NOT_FOUND' && /offline/i.test(error.message));
  } finally { rmSync(root, {recursive: true, force: true}); }
});


test('default model matches Rust constant and requires complete pins', {skip: !supported}, async () => {
  const native = createRequire(import.meta.url)('../prebuilds/darwin-arm64/zeppelin_embed.node');
  assert.deepEqual(TextStore.defaultModel, native.textDefaultModel());
  for (const [key, value] of Object.entries(TextStore.defaultModel)) {
    await assert.rejects(TextStore.openModel({path: '/unused', [key]: value, offline: true}), /all three/);
  }
  const root = mkdtempSync(join(tmpdir(), 'ze500-'));
  try {
    await assert.rejects(TextStore.openModel({path: join(root, 'store'), cacheDir: join(root, 'cache'), offline: true}),
      error => error.code === 'ZE_ERR_NOT_FOUND');
  } finally { rmSync(root, {recursive: true, force: true}); }
});
