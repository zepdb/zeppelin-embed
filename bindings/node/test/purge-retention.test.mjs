import assert from 'node:assert/strict';
import { mkdtempSync, readdirSync, readFileSync, rmSync } from 'node:fs';
import { tmpdir } from 'node:os';
import { join } from 'node:path';
import { createRequire } from 'node:module';
import test from 'node:test';

const { openNamespace, ZeppelinError } = createRequire(import.meta.url)('..');
const options = { durability: 'durable', commitTier: 'durable' };
const marker = 'ze246removedsentinel';
const survivor = 'ze246survivorsentinel';
function hits(root, text) {
  return readdirSync(root, { withFileTypes: true }).flatMap(entry => {
    const path = join(root, entry.name);
    return entry.isDirectory() ? hits(path, text) : readFileSync(path).includes(text) ? [path] : [];
  });
}
function fixture(fn) {
  const root = mkdtempSync(join(tmpdir(), 'ze246-'));
  let store = openNamespace(root, 'notes', {}, options);
  try {
    fn(store, root, () => {
      store.close();
      store = openNamespace(root, 'notes', {}, options);
      return store;
    }, () => { store.close(); store = undefined; });
  } finally {
    store?.close();
    rmSync(root, { recursive: true, force: true });
  }
}
const code = expected => error => error instanceof ZeppelinError && error.code === expected;

test('deferred purge removes deleted and superseded text from every file', () => fixture((store, root, reopen) => {
  store.upsert([{ id: 1n, text: marker }]);
  store.seal();
  store.upsert([{ id: 1n, text: `${marker}replacement` }]);
  store.delete([1n]);
  store.upsert([{ id: 2n, text: survivor }]);
  assert.notDeepEqual(hits(root, marker), []);
  const token = store.purge([1n, 999n], { wait: false });
  assert.equal(typeof token.tokenId, 'bigint');
  assert.equal(token.unknownIdCount, 1n);
  const report = store.awaitPurge(token.tokenId);
  assert.equal(report.walRewritten, true);
  assert.equal(report.unknownIdCount, 1n);
  assert.deepEqual(hits(root, marker), []);
  assert.throws(() => store.awaitPurge(token.tokenId), code('ZE_ERR_INVALID_ARGUMENT'));
  store = reopen();
  assert.deepEqual(hits(root, marker), []);
  assert.equal(store.get([2n], { text: true }).documents[0].text, survivor);
  const noop = store.purge([999n], { wait: false });
  assert.equal(store.awaitPurge(noop.tokenId).isNoOp, true);
}));

for (const method of ['dropPartition', 'applyRetention']) {
  const request = method === 'dropPartition' ? { start: 0n, end: 10n } : { window: 10n, nowTs: 20n };
  test(`${method} removes selected text from every file`, () => fixture((store, root, reopen) => {
    store.upsert([{ id: 1n, text: marker, timestamp: 5n }]);
    store.seal();
    store.upsert([{ id: 2n, text: survivor, timestamp: 10n }]);
    store.seal();
    // This segment crosses the cutoff and must remain intact.
    store.upsert([{ id: 3n, text: 'straddlerold', timestamp: 9n }, { id: 4n, text: 'straddlernew', timestamp: 11n }]);
    store.seal();
    store.upsert([{ id: 5n, text: 'activeold', timestamp: 1n }]);
    assert.notDeepEqual(hits(root, marker), []);
    const report = store[method](request);
    assert.equal(report.segmentsDropped, 1n);
    assert.equal(report.straddlersSkipped, 1n);
    assert.ok(report.bytesReclaimed > 0n);
    assert.equal(report.isNoOp, false);
    assert.deepEqual(hits(root, marker), []);
    assert.equal(store.count().count, 4n);
    assert.equal(store[method](request).isNoOp, true);
    store = reopen();
    assert.deepEqual(hits(root, marker), []);
    assert.equal(store.count().count, 4n);
    assert.equal(store.get([2n], { text: true }).documents[0].text, survivor);
  }));
}

test('lifecycle input and token validation', () => fixture((store, root, _reopen, close) => {
  assert.throws(() => store.purge([], { wait: 0 }), /boolean/);
  for (const invalid of [undefined, {}, { start: 0n }, { start: 0, end: 1n }, { start: -(1n << 64n), end: 1n }]) {
    assert.throws(() => store.dropPartition(invalid));
  }
  assert.throws(() => store.dropPartition({ start: 1n, end: 1n }), code('ZE_ERR_INVALID_ARGUMENT'));
  for (const invalid of [undefined, {}, { window: 1n }, { window: 1, nowTs: 2n }]) {
    assert.throws(() => store.applyRetention(invalid));
  }
  assert.throws(() => store.applyRetention({ window: 0n, nowTs: 2n }), code('ZE_ERR_INVALID_ARGUMENT'));
  for (const invalid of [undefined, -1n, 1, 1n << 64n]) assert.throws(() => store.awaitPurge(invalid));
  assert.throws(() => store.awaitPurge(999n), code('ZE_ERR_INVALID_ARGUMENT'));
  const token = store.purge([999n], { wait: false });
  const other = openNamespace(root, 'other', {}, options);
  try { assert.throws(() => other.awaitPurge(token.tokenId), code('ZE_ERR_INVALID_ARGUMENT')); } finally { other.close(); }
  store.awaitPurge(token.tokenId);
  close();
  for (const [method, arg] of [['awaitPurge', token.tokenId], ['dropPartition', { start: 0n, end: 1n }], ['applyRetention', { window: 1n, nowTs: 2n }]]) {
    assert.throws(() => store[method](arg));
  }
  const reader = openNamespace(root, 'notes', {}, { ...options, readOnly: true });
  try {
    assert.throws(() => reader.dropPartition({ start: 0n, end: 1n }), code('ZE_ERR_ACCESS_MODE'));
    assert.throws(() => reader.applyRetention({ window: 1n, nowTs: 2n }), code('ZE_ERR_ACCESS_MODE'));
    assert.throws(() => reader.awaitPurge(token.tokenId), code('ZE_ERR_INVALID_ARGUMENT'));
  } finally { reader.close(); }
}));
