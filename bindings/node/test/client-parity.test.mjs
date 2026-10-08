// Synthetic corpus generated from ADR-017 assumptions, NOT the client's recorded queries:
// replace or extend with the client's real corpus when provided.
import assert from 'node:assert/strict';
import { existsSync, mkdtempSync, readFileSync, rmSync } from 'node:fs';
import { tmpdir } from 'node:os';
import { join } from 'node:path';
import test from 'node:test';
import api from '../index.js';
import * as parity from '../bench/client-parity.mjs';
import { decodeRequest, SPEC } from '../bench/client-parity-generator.mjs';
import { replay, compare, fixtureDirectory } from '../bench/client-parity.mjs';

const expected = () => JSON.parse(readFileSync(new URL('expected.json', fixtureDirectory), 'utf8'));
const queries = JSON.parse(readFileSync(new URL('queries.json', fixtureDirectory), 'utf8')).queries;
let root, store;
function parityStore() {
  if (!store) {
    root = mkdtempSync(join(tmpdir(), 'ze367-test-'));
    store = parity.buildParityStore(api, root);
  }
  return store;
}
test.after(() => {
  try { store?.close(); } finally { if (root) rmSync(root, { recursive: true, force: true }); }
});

test('client_parity::release_translation_preserves_the_complete_note_eligible_set', () => {
  assert.equal(typeof parity.releaseCompatibleRequest, 'function', 'release translator is required');
  const originalQueries = structuredClone(queries);
  const translated = queries.map(({ name, request }) => ({ name, request: parity.releaseCompatibleRequest(request) }));
  const affected = queries.filter(({ request }) => request.eligibleIds);
  assert.equal(affected.length, 50);
  for (const { request } of affected) {
    const result = parity.releaseCompatibleRequest(request);
    assert.equal(Object.hasOwn(result, 'eligibleIds'), false);
    assert.deepEqual(result.filter.children[0], request.filter);
    const notePredicate = result.filter.children[1];
    const page = parityStore().scan({ filter: decodeRequest({ filter: notePredicate }).filter, limit: 303 });
    assert.equal(page.cursor, null);
    assert.deepEqual(page.documents.map(d => String(d.id)).sort((a, b) => Number(a) - Number(b)), request.eligibleIds);
  }
  for (const { request } of queries.filter(({ request }) => !request.eligibleIds)) {
    assert.deepEqual(parity.releaseCompatibleRequest(request), request);
  }
  const original = affected[0].request;
  const malformed = [[], original.eligibleIds.slice(1), [...original.eligibleIds, '999999'], [...original.eligibleIds].reverse(),
    original.eligibleIds.map((id, i) => i === 1 ? original.eligibleIds[0] : id),
    original.eligibleIds.map(id => String(BigInt(id) + 1n)),
    Array.from({ length: 302 }, (_, i) => String(151001 + i)),
    original.eligibleIds.map((id, i) => i === 0 ? 'invalid' : id)];
  for (const eligibleIds of malformed) assert.throws(() => parity.releaseCompatibleRequest({ ...original, eligibleIds }));
  assert.deepEqual(queries, originalQueries, 'translation must not mutate inputs');
  parity.compare(parity.replayQueries(parityStore(), affected), parity.replayQueries(parityStore(), translated.filter(q => q.request.filter?.op === 'and')));
});

let unifiedResult;
test('client_parity::moxie_queries_return_the_same_hits_on_the_unified_store', () => {
  assert.equal(typeof parity.replayUnified, 'function', 'unified replay is required');
  unifiedResult = parity.replayUnified(api, queries);
  compare(expected(), unifiedResult);
});

test('client parity matches saved published 0.6.0 reference', t => {
  const reference = new URL('expected-v0.6.0.json', fixtureDirectory);
  if (!existsSync(reference)) {
    t.skip('Published 0.6.0 reference absent. On the host run: node bindings/node/bench/client-parity.mjs release-reference <package-dir> tests/fixtures/client-parity-v1/expected-v0.6.0.json');
    return;
  }
  compare(JSON.parse(readFileSync(reference, 'utf8')), unifiedResult ?? parity.replayUnified(api, queries));
});

test('client_parity::cypher_eligible_then_query_matches_folder_filter', () => {
  assert.equal(typeof parity.folderEligibleIds, 'function', 'Cypher folder eligibility is required');
  parity.attachParityRelationships(parityStore());
  store.close(); store = undefined;
  store = api.openNamespace(root, 'notes', SPEC, { readOnly: true });
  const allFolders = Array.from({ length: 10 }, (_, i) => BigInt(i));
  const corpusIds = Array.from({ length: 151000 }, (_, i) => BigInt(i + 1));
  const byFolder = new Map();
  for (const folder of allFolders) {
    const ids = parity.folderEligibleIds(store, [folder]);
    // The seeded corpus assigns every row of note n to folder n % 10.
    assert.deepEqual(ids, corpusIds.filter(id => ((id - 1n) / 302n) % 10n === folder), 'complete seeded folder ID set');
    byFolder.set(folder, ids);
  }
  const folderIds = folders => [...new Set(folders)].flatMap(folder => byFolder.get(folder))
    .sort((a, b) => a < b ? -1 : a > b ? 1 : 0);
  assert.deepEqual(folderIds(allFolders), corpusIds, 'complete ordered union of all folders');
  assert.deepEqual(parity.folderEligibleIds(store, []), []);
  assert.deepEqual(parity.folderEligibleIds(store, [99n]), []);
  assert.deepEqual(parity.folderEligibleIds(store, [0n, 1n, 0n]), folderIds([0n, 1n]));
  const equivalent = request => {
    const folders = request.filter.values.map(value => BigInt(value.value));
    let eligibleIds = folderIds(folders);
    if (request.eligibleIds) {
      const existing = new Set(request.eligibleIds.map(BigInt));
      eligibleIds = eligibleIds.filter(id => existing.has(id));
    }
    const { filter, ...withoutFolder } = request;
    const translated = { ...withoutFolder, eligibleIds: eligibleIds.map(String) };
    const original = parity.replayQueries(store, [{ name: 'folder-parity', request }]);
    compare(original, parity.replayQueries(store, [{ name: 'folder-parity', request: translated }]));
    return original.results[0].hits;
  };
  const folderQueries = queries.filter(q => q.request.filter?.attributeId === 2);
  assert.equal(folderQueries.length, 100);
  for (const { request } of folderQueries) equivalent(request);
  const filter = { op: 'in', attributeId: 2, values: [{ id: 2, type: 'u64', value: '0' }] };
  const base = { text: 'plan1', k: 20, snippetBytes: 64, filter };
  assert.deepEqual(equivalent({ ...base, eligibleIds: [] }), []);
  assert.deepEqual(equivalent({ ...base, eligibleIds: ['304', '999999'] }), []);
  assert.deepEqual(equivalent({ ...base, eligibleIds: ['2', '2', '304', '999999'] }).map(hit => hit.id), ['2']);
  for (const alpha of [0, 0.5, 1]) equivalent({ ...base, vector: queries[300].request.vector, tier: 'exact', alpha });
});

test('client parity folder eligibility rejects incomplete results', () => {
  const one = ['00000000000000000000000000000001'], two = ['00000000000000000000000000000002'];
  const complete = { admittedGeneration: 1n, columns: ['id'], rows: [one, two] };
  const run = result => parity.folderEligibleIds({
    count: () => ({ generation: 1n, count: 2n }), cypher: () => result,
  }, [0n]);
  assert.deepEqual(run(complete), [1n, 2n]);
  assert.throws(() => run({ ...complete, rows: [one] }), /complete folder/);
  assert.throws(() => run({ ...complete, admittedGeneration: 2n }), /one generation/);
  assert.throws(() => run({ ...complete, rows: [one, one] }), /strictly ordered/);
});
test('client parity synthetic ADR-017 baseline matches current Store', () => {
  compare(expected(), replay());
});

test('client parity comparator enforces each recorded field and fused tolerance', () => {
  // Independent literal control: the comparator is not its own oracle.
  const hit = { id: '1', lexicalBm25: 2, vectorSquaredL2: null, fused: null,
    snippetBytes: 'YWJj', highlights: [{ start: 0, end: 3 }],
    sourceByteStart: 0, sourceByteEnd: 3, truncatedStart: false, truncatedEnd: false };
  const baseline = { schema: 'zeppelin-client-parity-v1', results: [
    { name: 'home', mode: 'lexical', hits: [hit, { ...hit, id: '2' }] },
    { name: 'chat', mode: 'hybrid', hits: [{ ...hit, vectorSquaredL2: 0.25, fused: 0.5 }] },
  ] };
  const mutate = (change) => {
    const actual = structuredClone(baseline);
    change(actual.results[0].hits[0]);
    assert.throws(() => compare(baseline, actual));
  };
  mutate(hit => { hit.id = '999999'; });
  mutate(hit => { hit.lexicalBm25 += 1e-12; });
  mutate(hit => { hit.vectorSquaredL2 = 1; });
  mutate(hit => { hit.snippetBytes = [0]; });
  mutate(hit => { hit.highlights = [{ start: 999, end: 1000 }]; });
  mutate(hit => { hit.sourceByteStart = 1; });
  mutate(hit => { hit.truncatedEnd = true; });
  const hybrid = baseline.results.findIndex(result => result.mode === 'hybrid');
  const actual = structuredClone(baseline);
  actual.results[hybrid].hits[0].fused += 0.5e-6;
  compare(baseline, actual);
  actual.results[hybrid].hits[0].fused += 2e-6;
  assert.throws(() => compare(baseline, actual));
  const reversed = structuredClone(baseline);
  reversed.results[0].hits.reverse();
  assert.throws(() => compare(baseline, reversed));
});

test('client parity committed queries match the seeded generator', async () => {
  const { queryShapes } = await import('../bench/client-parity-generator.mjs');
  const recorded = JSON.parse(readFileSync(new URL('queries.json', fixtureDirectory), 'utf8'));
  assert.equal(recorded.queries.length, 500);
  assert.deepEqual(queryShapes(), recorded.queries);
});
