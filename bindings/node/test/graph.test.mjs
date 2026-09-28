import assert from 'node:assert/strict';
import { mkdtempSync, rmSync, existsSync, readdirSync, statSync } from 'node:fs';
import { tmpdir, release } from 'node:os';
import { join } from 'node:path';
import { createRequire } from 'node:module';
import test from 'node:test';
import { spawnSync } from 'node:child_process';
const { GraphStore, ZeppelinError } = createRequire(import.meta.url)('..');
const supported = (process.platform === 'darwin' && Number.parseInt(release(), 10) >= 23) ||
  (process.platform === 'win32' && process.arch === 'x64');
const node = (key, extra = {}) => ({ kind: 'node', operation: 'create', namespace: 'docs', key, revision: 1n, labels: ['Doc'], ...extra });
function fixture(fn) {
  const dir = mkdtempSync(join(tmpdir(), 'ze-node-graph-'));
  const path = join(dir, 'graph');
  const store = GraphStore.open(path);
  try { fn(store, path); } finally { store.close(); rmSync(dir, { recursive: true, force: true }); }
}
test('graph support and lifecycle', () => {
  assert.equal(typeof GraphStore, 'function');
  assert.equal(GraphStore.isSupported(), supported);
  if (!supported) {
    assert.throws(() => GraphStore.open('unused'), e => e instanceof ZeppelinError && e.code === 'ZE_ERR_UNSUPPORTED');
    return;
  }
  fixture(s => { s.close(); s.close(); assert.throws(() => s.cypher('RETURN 1'), e => e.code === 'ZE_ERR_CLOSED'); });
});
test('atomic node/relationship round trip and reopen', { skip: !supported }, () => fixture((s, path) => {
  const result = s.apply([node('a', { text: 'hello', properties: { title: 'Alpha', count: 7n, weight: 1.5, active: true, tags: ['a', 'b'] } }), node('b'),
    { kind: 'relationship', operation: 'create', namespace: 'edges', key: 'ab', revision: 1n, type: 'LINKS', source: { local: 0 }, target: { local: 1 }, properties: { strength: 2n } }]);
  assert.equal(result.disposition, 'Committed'); assert.equal(result.generation, 1n); assert.equal(result.receipts.length, 3);
  const read = s.cypher('MATCH (a:Doc)-[r:LINKS]->(b:Doc) RETURN a, r, b, ze.stored_text(a)');
  assert.equal(read.rows.length, 1);
  const [a, r, b, text] = read.rows[0];
  assert.equal(a.kind, 'node'); assert.equal(a.key, 'a'); assert.equal(a.namespace, 'docs');
  assert.deepEqual(a.properties, { title: 'Alpha', count: 7n, weight: 1.5, active: true, tags: ['a', 'b'] });
  assert.equal(r.kind, 'relationship'); assert.equal(r.source, a.id); assert.equal(r.target, b.id); assert.equal(r.type, 'LINKS'); assert.equal(text, 'hello');
  s.close(); const reopened = GraphStore.open(path, { mode: 'readWrite' });
  try { assert.deepEqual(reopened.cypher('MATCH (n:Doc) RETURN count(n)').rows, [[2n]]); } finally { reopened.close(); }
}));
test('exact retry reports Replayed', { skip: !supported }, () => fixture(s => {
  const items = [node('retry')]; const first = s.apply(items); const retry = s.apply(items);
  assert.equal(retry.disposition, 'Replayed'); assert.equal(retry.receipts[0].id, first.receipts[0].id);
  assert.deepEqual(s.cypher('MATCH (n) RETURN count(n)').rows, [[1n]]);
}));
test('atomic failure mid-batch leaves nothing', { skip: !supported }, () => fixture(s => {
  assert.throws(() => s.apply([node('a'), { kind: 'relationship', operation: 'create', namespace: 'edges', key: 'bad', revision: 1n, type: 'LINKS', source: { local: 0 }, target: 999n }]),
    e => e instanceof ZeppelinError && e.disposition === 'NotCommitted');
  assert.deepEqual(s.cypher('MATCH (n) RETURN count(n)').rows, [[0n]]);
}));
test('malformed input is rejected before effect', { skip: !supported }, () => fixture(s => {
  for (const bad of [{ properties: { n: 1n << 63n } }, { properties: { n: NaN } }, { labels: 'bad' }, { revision: -1n }, { vector: [1] }, { operation: 'typo' }]) {
    assert.throws(() => s.apply([node('good'), node('bad', bad)]), e => e instanceof ZeppelinError && e.code === 'ZE_ERR_INVALID_ARGUMENT' && e.disposition === 'NotCommitted');
  }
  assert.throws(() => s.cypher('CREATE (n:Doc) RETURN n', { unsupported: [] }), e => e.code === 'ZE_ERR_INVALID_ARGUMENT');
  assert.deepEqual(s.cypher('MATCH (n) RETURN count(n)').rows, [[0n]]);
}));
test('typed scalar parameters and list results', { skip: !supported }, () => fixture(s => {
  const r = s.cypher('RETURN $nil AS nil, $b AS b, $i AS i, $f AS f, $s AS s, [1, null, true] AS xs', { nil: null, b: true, i: -9007199254740993n, f: 1.25, s: 'héllo\0世界' });
  assert.deepEqual(r.columns, ['nil', 'b', 'i', 'f', 's', 'xs']);
  assert.deepEqual(r.rows, [[null, true, -9007199254740993n, 1.25, 'héllo\0世界', [1n, null, true]]]);
}));
test('row limits fail without truncation and allow more than 1024 rows', { skip: !supported }, () => fixture(s => {
  s.apply(Array.from({ length: 33 }, (_, i) => node(String(i))));
  for (const options of [undefined, { maxRows: 0 }, { maxRows: 1024 }, { maxRows: 1 }]) {
    assert.throws(() => s.cypher('MATCH (a), (b) RETURN 7 AS k', {}, options), e => e instanceof ZeppelinError && e.code === 'ZE_ERR_BUDGET_EXCEEDED');
  }
  assert.equal(s.cypher('MATCH (a), (b) RETURN 7 AS k', {}, { maxRows: 1089 }).rows.length, 1089);
  for (const maxRows of [-1, 65537, 1.5, NaN]) assert.throws(() => s.cypher('CREATE (n) RETURN n', {}, { maxRows }), e => e.code === 'ZE_ERR_INVALID_ARGUMENT');
  assert.deepEqual(s.cypher('MATCH (n) RETURN count(n)').rows, [[33n]]);
}));

test('graph support refuses macOS below 14', { skip: process.platform !== 'darwin' }, () => {
  const result = spawnSync(process.execPath, ['-e', `
    require('node:os').release = () => '22.6.0';
    const assert = require('node:assert/strict');
    const { GraphStore } = require(${JSON.stringify(new URL('../index.js', import.meta.url).pathname)});
    assert.equal(GraphStore.isSupported(), false);
    assert.throws(() => GraphStore.open('unused'), e => e.code === 'ZE_ERR_UNSUPPORTED');
  `], { encoding: 'utf8' });
  assert.equal(result.status, 0, result.stderr);
});


test('autoReclaim validates before open', { skip: !supported }, () => {
  const dir = mkdtempSync(join(tmpdir(), 'ze-maintenance-options-'));
  const path = join(dir, 'graph');
  try {
    for (const options of [{ autoReclaim: 1 }, { autoReclaim: null },
      { reclaimAfterBytes: 0 }, { reclaimAfterBytes: 1048575 },
      { reclaimAfterBytes: 1048576.5 }, { reclaimAfterBytes: Number.MAX_SAFE_INTEGER + 1 },
      { mode: 'readOnly', autoReclaim: false }, { mode: 'readOnly', reclaimAfterBytes: 1048576 }]) {
      assert.throws(() => GraphStore.open(path, options), e => e.code === 'ZE_ERR_INVALID_ARGUMENT');
      assert.equal(existsSync(path), false);
    }
    const store = GraphStore.open(path, { autoReclaim: false, reclaimAfterBytes: 1048576 });
    store.close();
  } finally { rmSync(dir, { recursive: true, force: true }); }
});

test('maintain() returns a report and reclaims a dead pack', { skip: !supported }, async t => {
  const dir = mkdtempSync(join(tmpdir(), 'ze-maintenance-'));
  const path = join(dir, 'graph');
  let store = GraphStore.open(path);
  try {
    assert.equal(typeof store.maintain, 'function');
    store.close();
    store = GraphStore.open(path, { mode: 'readWrite', autoReclaim: false });
    let ids;
    // Revisions make old packs dead without relying on the concurrent S6 drain work.
    for (let batch = 0; batch < 20; ++batch) {
      const result = store.apply(Array.from({ length: 10 }, (_, i) => node(`n${i}`, batch === 0 ? {} : {
        operation: 'put', expectedId: ids[i], revision: BigInt(batch + 1), properties: { revision: BigInt(batch) },
      })));
      ids = result.receipts.map(receipt => receipt.id);
    }
    store.close();
    store = GraphStore.open(path, { mode: 'readWrite', autoReclaim: false });
    const census = () => new Map(readdirSync(path).map(file => [file, statSync(join(path, file)).size]));
    const before = census();
    let report;
    let removed = 0n;
    // A preparation can finish without an intent; reclamation follows publication.
    for (let step = 0; step < 8 * 4; ++step) {
      report = store.maintain();
      for (const field of ['generation', 'replacedPhysicalRefs', 'newPackBytes', 'relocatedBytes', 'drainedPacks', 'reclaimedBytes', 'removedBytes']) assert.equal(typeof report[field], 'bigint', field);
      assert.equal(typeof report.cycleComplete, 'boolean');
      removed += report.removedBytes;
      if (report.cycleComplete && removed > 0n) break;
    }
    assert.equal(report.cycleComplete, true);
    const after = census();
    const bytes = files => [...files.values()].reduce((a, b) => a + b, 0);
    t.diagnostic(`bytes ${bytes(before)} -> ${bytes(after)}; files ${before.size} -> ${after.size}; removed ${removed}`);
    assert.ok(bytes(after) < bytes(before), `bytes ${bytes(before)} -> ${bytes(after)}, removed ${removed}`);
    assert.ok([...before.keys()].some(file => !after.has(file)), 'a preexisting dead artifact was removed');
    // File count falls from the second completed cycle on (ZE-260 S6 evidence).
    assert.ok(removed > 0n);
    assert.deepEqual(store.cypher('MATCH (n) RETURN count(n)').rows, [[10n]]);
    assert.equal(typeof (await store.maintainAsync()).cycleComplete, 'boolean');
  } finally { store.close(); rmSync(dir, { recursive: true, force: true }); }
});
