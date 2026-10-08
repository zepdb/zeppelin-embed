import assert from 'node:assert/strict';
import test from 'node:test';
import { mkdtempSync, rmSync, readdirSync, readFileSync } from 'node:fs';
import { tmpdir } from 'node:os';
import { join } from 'node:path';
import api from '../index.js';
import {spawnSync} from 'node:child_process';
const { Store, openNamespace } = api;
const node = (key, extra = {}) => ({kind: 'node', operation: 'create', namespace: 'docs', key, revision: 1n, ...extra});
async function fixture(run) {
  const root = mkdtempSync(join(tmpdir(), 'ze361-'));
  const s = openNamespace(root, 'graph', {vectorSpace: {dimensions: 2}, attributes: [{id: 1, name: 'rank', type: 'i64', nullable: true}]}, {maxResidentBytes: 268435456n});
  try { s.enableGraph(); await run(s); } finally { s.close(); rmSync(root, {recursive: true, force: true}); }
}
test('store graph: legacy class is absent and capability checks do not open', () => {
  assert.equal(api.GraphStore, undefined);
  assert.equal(typeof Store.graphSupported, 'function');
  for (const name of ['enableGraph', 'graphApply', 'graphQuery', 'graphGetNodes', 'graphGetRelationships', 'graphResources', 'cypher']) assert.equal(typeof Store.prototype[name], 'function', name);
});
test('store graph: one handle shares documents, vectors, attributes and relationships', () => fixture(s => {
  const id = (1n << 100n) + 7n;
  const vector = new Float32Array([1, 0]);
  const r = s.graphApply([node('a', {id, labels: ['Chosen'], text: 'amber cedar', vector, timestamp: 9n, attributes: [{id: 1, type: 'i64', value: 4n}], metadata: new Uint8Array([2, 3])}), node('b'), {kind: 'relationship', operation: 'create', namespace: 'edges', key: 'ab', revision: 1n, type: 'LINK', source: {local: 0}, target: {local: 1}}]);
  assert.equal(r.receipts[0].id, id);
  assert.deepEqual(s.cypher('MATCH (n) WHERE n.key IN $xs RETURN n.key', {xs: ['unused']}).rows, []);
  assert.deepEqual(s.cypher('RETURN $xs', {xs: [[], [1n, null, 'x']]}).rows, [[[[], [1n, null, 'x']]]]);
  const docs = s.get([id], {text: true, vector: true, metadata: true, attributes: true});
  assert.equal(docs.documents[0].text, 'amber cedar');
  assert.deepEqual(docs.documents[0].vector, vector);
  assert.equal(docs.documents[0].timestamp, 9n);
  assert.equal(docs.documents[0].attributes[0].value, 4n);
  assert.deepEqual(docs.documents[0].metadata, new Uint8Array([2, 3]));
  assert.deepEqual(s.query({text: 'amber', eligibleIds: [id]}).hits.map(h => h.id), [id]);
  const rows = s.graphGetNodes([id, 999n, id], {text: true, vector: true});
  assert.equal(rows[0].id, id); assert.equal(rows[1], null);
  assert.deepEqual(rows[2].vector, vector);
  rows[0].vector[0] = 9;
  assert.deepEqual(s.graphGetNodes([id], {vector: true})[0].vector, vector);
  assert.equal(s.graphGetRelationships([r.receipts[2].id])[0].source, id);
  const plan = {root: 2, operators: [['scanNodes', 0, null], ['filter', 0, 1], ['project', 1, [{slot: 1, expression: 0}]]], expressions: [['slot', 0], ['hasLabel', 0, 'Chosen']], parameters: [], searches: [], eagerSearches: []};
  assert.deepEqual(s.graphQuery(plan).rows.map(row => row[0].id), [id]);
  const search = {root: 0, operators: [['search', 0, null]], expressions: [['literal', 1n], ['literal', new Float32Array([1, 0])]], parameters: [], searches: [{kind: ['vector', 1], call: 0, k: 0, tier: 1, node: 0, score: 1}], eagerSearches: [0]};
  assert.equal(s.graphQuery(search).rows[0][0].id, id);
  const lists = {root: 1, operators: [['unit'], ['project', 0, [{slot: 0, expression: 0}]]], expressions: [['literal', [1n, [null, 'copied']]]], parameters: [], searches: [], eagerSearches: []};
  assert.deepEqual(s.graphQuery(lists).rows, [[[1n, [null, 'copied']]]]);
  const params = {...lists, expressions: [['parameter', 0]], parameters: [{name: 'xs', kinds: 128}]};
  const xs = [1n, ['before']];
  const pending = s.graphQueryAsync(params, {parameters: {xs}}); xs[1][0] = 'changed';
  for (const value of Object.values(s.graphResources())) assert.equal(typeof value, 'bigint');
  assert.equal(typeof s.enableGraph().generation, 'bigint');
  return pending.then(result => { assert.deepEqual(result.rows, [[[1n, ['before']]]]); });
}));
test('store graph: async requests own inputs and share cancellation', () => fixture(async s => {
  const v = new Float32Array([1, 0]); const metadata = new Uint8Array([7]);
  const pending = s.graphApplyAsync([node('async', {id: 77n, text: 'owned', vector: v, metadata})]);
  v[0] = 8; metadata[0] = 9;
  await pending;
  assert.deepEqual(s.graphGetNodes([77n], {vector: true})[0].vector, new Float32Array([1, 0]));
  const c = new AbortController(); c.abort();
  await assert.rejects(s.graphApplyAsync([node('aborted')], {signal: c.signal}), e => e.code === 'ZE_ERR_CANCELLED' && e.disposition === 'NotCommitted');
  await assert.rejects(s.cypherAsync('RETURN 1', {}, {signal: c.signal}), e => e.code === 'ZE_ERR_CANCELLED');
  await assert.rejects(s.graphQueryAsync({root: 0, operators: [['unit']], expressions: [], parameters: [], searches: [], eagerSearches: []}, {signal: c.signal}), e => e.code === 'ZE_ERR_CANCELLED');

}));
test('store graph: invalid fields and empty batches leave graph-free bytes unchanged', () => {
  const root = mkdtempSync(join(tmpdir(), 'ze361-free-'));
  const path = join(root, 'store');
  const s = new Store(path, {maxResidentBytes: 268435456n});
  const files = () => Object.fromEntries(readdirSync(path).sort().map(name => [name, readFileSync(join(path, name)).toString('hex')]));
  try {
    const before = files();
    for (const extra of [{vector: [1]}, {timestamp: 1n << 63n}, {id: 1n << 128n}, {operation: 'delete', expectedId: 1n, vector: new Float32Array([1, 0])}]) assert.throws(() => s.graphApply([node('bad', extra)]), e => e.code === 'ZE_ERR_INVALID_ARGUMENT');
    assert.throws(() => s.graphQuery('MATCH (n) RETURN n'));
    // The current C entry rejects graph-free apply before automatic enable.
    // Direct C reproduction is recorded in the ticket evidence.
    assert.throws(() => s.graphApply([]), e => e.code === 'ZE_ERR_INTERNAL' && e.disposition === 'NotCommitted');
    assert.deepEqual(files(), before);
  } finally { s.close(); rmSync(root, {recursive: true, force: true}); }
});
test('store graph: controls reject conflicts and nested inputs are copied', () => fixture(async s => {
  const c = new AbortController();
  await assert.rejects(s.graphApplyAsync([node('bad-control')], {signal: c.signal, deadlineNs: 1n}), e => e.code === 'ZE_ERR_INVALID_ARGUMENT');
  assert.throws(() => s.graphApply([node('bad-dimension', {vector: new Float32Array([1])})]), e => e.disposition === 'NotCommitted');
  assert.deepEqual(s.cypher('MATCH (n) RETURN count(n)').rows, [[0n]]);
  const items = [node('nested', {id: 88n, vector: new Float32Array([1, 0]), metadata: new Uint8Array([7]), attributes: [{id: 1, type: 'i64', value: 5n}], properties: {xs: ['a', 'b']}, labels: ['Before']})];
  const pending = s.graphApplyAsync(items);
  items[0].properties.xs[0] = 'changed'; items[0].labels[0] = 'Changed'; items[0].attributes[0].value = 99n; items[0].metadata[0] = 9;
  await pending;
  const result = await s.graphGetNodesAsync([88n, 99n, 88n], {vector: true});
  assert.deepEqual(result[0].properties.xs, ['a', 'b']);
  assert.deepEqual(result[0].labels, ['Before']);
  const doc = s.get([88n], {metadata: true, attributes: true}).documents[0];
  assert.deepEqual(doc.metadata, new Uint8Array([7])); assert.equal(doc.attributes[0].value, 5n);
  assert.equal((await s.graphGetRelationshipsAsync([99n]))[0], null);
}));

test('store graph: unsupported builds reject before looking up native graph methods', () => {
  const child = spawnSync(process.execPath, ['-e', `
    const assert = require('node:assert/strict');
    require('node:module')._extensions['.node'] = module => { module.exports = {graphSupported: false, abiVersion: 1}; };
    const {Store} = require(${JSON.stringify(new URL('../index.js', import.meta.url).pathname)});
    assert.equal(Store.graphSupported(), false);
    for (const name of ['enableGraph', 'graphResources', 'graphApply', 'graphQuery', 'cypher', 'graphGetNodes', 'graphGetRelationships']) assert.throws(() => Store.prototype[name].call(Object.create(Store.prototype)), e => e.code === 'ZE_ERR_GRAPH_UNSUPPORTED_BUILD');
    (async () => {
      for (const name of ['graphApplyAsync', 'graphQueryAsync', 'cypherAsync', 'graphGetNodesAsync', 'graphGetRelationshipsAsync']) await assert.rejects(Store.prototype[name].call(Object.create(Store.prototype)), e => e.code === 'ZE_ERR_GRAPH_UNSUPPORTED_BUILD');
    })().catch(e => { console.error(e); process.exitCode = 1; });
  `], {encoding: 'utf8'});
  assert.equal(child.status, 0, child.stderr);
});
