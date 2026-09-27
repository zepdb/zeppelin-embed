import test from 'node:test';
import assert from 'node:assert/strict';
import { mkdtempSync, rmSync } from 'node:fs';
import { tmpdir } from 'node:os';
import { join } from 'node:path';
import { GraphStore } from '../index.js';

const supported = process.platform === 'darwin' && process.arch === 'arm64';
const node = key => ({ kind: 'node', operation: 'create', namespace: 'docs', key, revision: 1n });
const edge = (key, type, source, target) => ({ kind: 'relationship', operation: 'create', namespace: 'edges', key, revision: 1n, type, source, target });
const deletion = (key, expectedId) => ({ kind: 'node', operation: 'delete', namespace: 'docs', key, expectedId, revision: 2n, detach: true });
const refused = e => e.disposition === 'NotCommitted';
function fixture(relationshipTypes, run) {
  const dir = mkdtempSync(join(tmpdir(), 'ze-integrity-'));
  const path = join(dir, 'graph');
  let store;
  try {
    store = GraphStore.open(path, relationshipTypes ? { relationshipTypes } : {});
    run(store, () => { store.close(); store = GraphStore.open(path, { mode: 'readWrite' }); return store; });
  } finally { store?.close(); rmSync(dir, { recursive: true, force: true }); }
}

test('dangling endpoint and same-batch endpoint deletion reject atomically', { skip: !supported }, () => fixture(null, s => {
  assert.throws(() => s.apply([node('a'), edge('missing', 'IN', { local: 0 }, 999n)]), refused);
  assert.deepEqual(s.cypher('MATCH (n) RETURN count(n)').rows, [[0n]]);
  const parent = s.apply([node('parent')]).receipts[0].id;
  assert.throws(() => s.apply([node('child'), edge('deleted', 'IN', { local: 0 }, parent), deletion('parent', parent)]), refused);
  assert.deepEqual(s.cypher('MATCH (n) RETURN count(n)').rows, [[1n]]);
}));

test('declared restrict rejects detach atomically after reopen', { skip: !supported }, () => fixture([{ type: 'IN', onDelete: 'restrict' }], (s, reopen) => {
  const r = s.apply([node('child'), node('parent'), edge('in', 'IN', { local: 0 }, { local: 1 })]);
  s = reopen();
  assert.throws(() => s.apply([node('unrelated'), deletion('parent', r.receipts[1].id)]), refused);
  assert.throws(() => s.cypher('MATCH (c)-[:IN]->(p) DETACH DELETE p'), refused);
  assert.deepEqual(s.cypher('MATCH (n) RETURN count(n)').rows, [[2n]]);
  assert.deepEqual(s.cypher('MATCH ()-[r]->() RETURN count(r)').rows, [[1n]]);
}));

test('declared cascade deletes transitive children atomically after reopen', { skip: !supported }, () => fixture([{ type: 'IN', onDelete: 'cascade' }], (s, reopen) => {
  const r = s.apply([node('leaf'), node('child'), node('parent'), edge('a', 'IN', { local: 0 }, { local: 1 }), edge('b', 'IN', { local: 1 }, { local: 2 })]);
  s = reopen();
  const result = s.apply([node('unrelated'), deletion('parent', r.receipts[2].id)]);
  assert.equal(result.receipts.length, 2);
  assert.throws(() => s.apply([node('leaf')]), refused); // implicit deletion retains the key fence
  assert.equal(s.apply([deletion('parent', r.receipts[2].id)]).disposition, 'Replayed');
  assert.deepEqual(s.cypher('MATCH (n) RETURN n').rows.map(row => row[0].key), ['unrelated']);
  s = reopen();
  assert.deepEqual(s.cypher('MATCH (n) RETURN count(n)').rows, [[1n]]);
  s.apply([node('cy-leaf'), node('cy-child'), { ...node('cy-parent'), labels: ['Parent'] },
    edge('cy-a', 'IN', { local: 0 }, { local: 1 }), edge('cy-b', 'IN', { local: 1 }, { local: 2 })]);
  s.cypher('MATCH (p:Parent) DELETE p');
  assert.deepEqual(s.cypher('MATCH (n) RETURN count(n)').rows, [[1n]]);
}));

test('mixed restrict blocks the entire cascade', { skip: !supported }, () => fixture([{ type: 'IN', onDelete: 'cascade' }, { type: 'REF', onDelete: 'restrict' }], s => {
  const r = s.apply([node('a'), node('b'), node('outside'), edge('ab', 'IN', { local: 0 }, { local: 1 }), edge('ba', 'IN', { local: 1 }, { local: 0 }), edge('ref', 'REF', { local: 2 }, { local: 0 })]);
  assert.throws(() => s.apply([deletion('b', r.receipts[1].id)]), refused);
  assert.deepEqual(s.cypher('MATCH (n) RETURN count(n)').rows, [[3n]]);
  // Removing the restrictive edge permits the cycle to cascade once per node.
  s.apply([{ kind: 'relationship', operation: 'delete', namespace: 'edges', key: 'ref', revision: 2n, expectedId: r.receipts[5].id }, deletion('b', r.receipts[1].id)]);
  assert.deepEqual(s.cypher('MATCH (n) RETURN n').rows.map(row => row[0].key), ['outside']);
}));
