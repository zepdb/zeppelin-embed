import { openGraph } from './graph-fixture.mjs';
import test from 'node:test';
import assert from 'node:assert/strict';
import { mkdtempSync, rmSync } from 'node:fs';
import { tmpdir } from 'node:os';
import { join } from 'node:path';
import { Store } from '../index.js';

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
    store = openGraph(path, relationshipTypes ? { relationshipTypes } : {});
    run(store, () => { store.close(); store = openGraph(path, {}); return store; });
  } finally { store?.close(); rmSync(dir, { recursive: true, force: true }); }
}

test('dangling endpoint and same-batch endpoint deletion reject atomically', { skip: !supported }, () => fixture(null, s => {
  assert.throws(() => s.graphApply([node('a'), edge('missing', 'IN', { local: 0 }, 999n)]), refused);
  assert.deepEqual(s.cypher('MATCH (n) RETURN count(n)').rows, [[0n]]);
  const parent = s.graphApply([node('parent')]).receipts[0].id;
  assert.throws(() => s.graphApply([node('child'), edge('deleted', 'IN', { local: 0 }, parent), deletion('parent', parent)]), refused);
  assert.deepEqual(s.cypher('MATCH (n) RETURN count(n)').rows, [[1n]]);
}));

test('declared restrict rejects detach atomically after reopen', { skip: !supported }, () => fixture([{ type: 'IN', onDelete: 'restrict' }], (s, reopen) => {
  const r = s.graphApply([node('child'), node('parent'), edge('in', 'IN', { local: 0 }, { local: 1 })]);
  s = reopen();
  assert.throws(() => s.graphApply([node('unrelated'), deletion('parent', r.receipts[1].id)]), refused);
  assert.throws(() => s.cypher('MATCH (c)-[:IN]->(p) DETACH DELETE p'), refused);
  assert.deepEqual(s.cypher('MATCH (n) RETURN count(n)').rows, [[2n]]);
  assert.deepEqual(s.cypher('MATCH ()-[r]->() RETURN count(r)').rows, [[1n]]);
}));

test('declared cascade deletes transitive children atomically after reopen', { skip: !supported }, () => fixture([{ type: 'IN', onDelete: 'cascade' }], (s, reopen) => {
  const r = s.graphApply([node('leaf'), node('child'), node('parent'), edge('a', 'IN', { local: 0 }, { local: 1 }), edge('b', 'IN', { local: 1 }, { local: 2 })]);
  s = reopen();
  const result = s.graphApply([node('unrelated'), deletion('parent', r.receipts[2].id)]);
  assert.equal(result.receipts.length, 2);
  assert.throws(() => s.graphApply([node('leaf')]), refused); // implicit deletion retains the key fence
  assert.equal(s.graphApply([deletion('parent', r.receipts[2].id)]).disposition, 'Replayed');
  assert.deepEqual(s.cypher('MATCH (n) RETURN n').rows.map(row => row[0].key), ['unrelated']);
  s = reopen();
  assert.deepEqual(s.cypher('MATCH (n) RETURN count(n)').rows, [[1n]]);
  s.graphApply([node('cy-leaf'), node('cy-child'), { ...node('cy-parent'), labels: ['Parent'] },
    edge('cy-a', 'IN', { local: 0 }, { local: 1 }), edge('cy-b', 'IN', { local: 1 }, { local: 2 })]);
  s.cypher('MATCH (p:Parent) DELETE p');
  assert.deepEqual(s.cypher('MATCH (n) RETURN count(n)').rows, [[1n]]);
}));

test('mixed restrict blocks the entire cascade', { skip: !supported }, () => fixture([{ type: 'IN', onDelete: 'cascade' }, { type: 'REF', onDelete: 'restrict' }], s => {
  const r = s.graphApply([node('a'), node('b'), node('outside'), edge('ab', 'IN', { local: 0 }, { local: 1 }), edge('ba', 'IN', { local: 1 }, { local: 0 }), edge('ref', 'REF', { local: 2 }, { local: 0 })]);
  assert.throws(() => s.graphApply([deletion('b', r.receipts[1].id)]), refused);
  assert.deepEqual(s.cypher('MATCH (n) RETURN count(n)').rows, [[3n]]);
  // Removing the restrictive edge permits the cycle to cascade once per node.
  s.graphApply([{ kind: 'relationship', operation: 'delete', namespace: 'edges', key: 'ref', revision: 2n, expectedId: r.receipts[5].id }, deletion('b', r.receipts[1].id)]);
  assert.deepEqual(s.cypher('MATCH (n) RETURN n').rows.map(row => row[0].key), ['outside']);
}));
