import assert from 'node:assert/strict';
import { mkdtempSync, readdirSync, rmSync } from 'node:fs';
import { tmpdir } from 'node:os';
import { join } from 'node:path';
import { createRequire } from 'node:module';
import test from 'node:test';
const { openNamespace } = createRequire(import.meta.url)('..');
const fast = { durability: 'durable', commitTier: 'none', autoSealRows: 1 };

test('autoMerge validates before open', () => {
  const root = mkdtempSync(join(tmpdir(), 'ze-auto-options-'));
  try {
    for (const autoMerge of [1, null, 'true', {}]) {
      assert.throws(() => openNamespace(root, 'notes', {}, { autoMerge }), /autoMerge/);
    }
    assert.throws(() => openNamespace(root, 'notes', {}, { autoMerge: true, readOnly: true }), /readOnly/);
    openNamespace(root, 'notes', {}, fast).close();
  } finally { rmSync(root, { recursive: true, force: true }); }
});

test('autoMerge consolidates revisions without manual maintenance', () => {
  const root = mkdtempSync(join(tmpdir(), 'ze-auto-merge-'));
  let store;
  const segments = () => readdirSync(join(root, 'notes')).filter(f => f.endsWith('.zseg')).length;
  try {
    store = openNamespace(root, 'notes', {}, fast);
    for (let revision = 1n; revision <= 5n; revision++) store.upsert([{ id: 1n, revision, text: `old ${revision}` }]);
    assert.equal(segments(), 4); // Default remains disabled.
    store.close();
    store = openNamespace(root, 'notes', {}, { ...fast, autoMerge: true });
    assert.equal(segments(), 1); // Open also consolidates existing segments.
    for (let revision = 6n; revision <= 30n; revision++) {
      store.upsert([{ id: 1n, revision, text: `latest ${revision}` }]);
      assert.equal(segments(), 1);
    }
    assert.equal(store.count().count, 1n);
    assert.equal(store.get([1n], { text: true }).documents[0].text, 'latest 30');
    assert.equal(store.seal().generation, store.merge().generation);
    store.close();
    store = openNamespace(root, 'notes', {}, { ...fast, autoMerge: true });
    assert.equal(store.get([1n]).documents[0].revision, 30n);
    assert.equal(segments(), 1);
  } finally { store?.close(); rmSync(root, { recursive: true, force: true }); }
});

test('autoMerge failure prevents the pending write', () => {
  const root = mkdtempSync(join(tmpdir(), 'ze-auto-failure-'));
  const store = openNamespace(root, 'notes', {}, { ...fast, autoMerge: true });
  const merge = store.merge;
  try {
    store.upsert([{ id: 1n, text: 'first' }]);
    store.merge = () => { throw new Error('injected merge failure'); };
    assert.throws(() => store.upsert([{ id: 2n, text: 'pending' }]), /injected merge failure/);
    assert.equal(store.count().count, 1n);
    assert.throws(() => store.upsert([{ id: 2n, text: 'retry' }]), /injected merge failure/);
    store.merge = merge;
    store.upsert([{ id: 2n, text: 'retry' }]);
    assert.equal(store.count().count, 2n);
  } finally { store.merge = merge; store.close(); rmSync(root, { recursive: true, force: true }); }
});
