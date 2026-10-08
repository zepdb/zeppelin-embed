// Synthetic corpus generated from ADR-017 assumptions, NOT the client's recorded queries:
// replace or extend with the client's real corpus when provided.
import assert from 'node:assert/strict';
import { mkdtempSync, readdirSync, readFileSync, realpathSync, rmSync, writeFileSync } from 'node:fs';
import { createHash } from 'node:crypto';
import { tmpdir } from 'node:os';
import { join, resolve, sep } from 'node:path';
import { createRequire } from 'node:module';
import { pathToFileURL } from 'node:url';
import { SPEC, seed, provenance, documentBatches, queryShapes, decodeRequest } from './client-parity-generator.mjs';
export const fixtureDirectory = new URL('../../../tests/fixtures/client-parity-v1/', import.meta.url);
const read = name => JSON.parse(readFileSync(new URL(name, fixtureDirectory), 'utf8'));
const write = (name, value) => writeFileSync(new URL(name, fixtureDirectory), `${JSON.stringify(value)}\n`);
function recordHit(hit, mode) {
  return {
    id: String(hit.id), lexicalBm25: hit.lexicalBm25 ?? null,
    vectorSquaredL2: hit.vectorSquaredL2 ?? null, fused: mode === 'hybrid' ? hit.score : null,
    snippetBytes: hit.snippet ? Buffer.from(hit.snippet.text, 'utf8').toString('base64') : null,
    highlights: hit.snippet?.highlights ?? null,
    sourceByteStart: hit.snippet?.sourceByteStart ?? null,
    sourceByteEnd: hit.snippet?.sourceByteEnd ?? null,
    truncatedStart: hit.snippet?.truncatedStart ?? null,
    truncatedEnd: hit.snippet?.truncatedEnd ?? null,
  };
}
// Accepts a package exporting openNamespace, or call replayQueries directly with
// any prepared Store. ZE-367 can enable graph/write relationships before replay.
export function replayQueries(store, queries = read('queries.json').queries) {
  return { schema: 'zeppelin-client-parity-v1', seed, provenance,
    results: queries.map(({ name, request }) => {
      const result = store.query(decodeRequest(request));
      return { name, mode: result.mode, hits: result.hits.map(hit => recordHit(hit, result.mode)) };
    }) };
}
export function releaseCompatibleRequest(request) {
  if (!Object.hasOwn(request, 'eligibleIds')) return { ...request };
  assert.ok(Array.isArray(request.eligibleIds) && request.eligibleIds.length === 302,
    'release translation requires one complete 302-document note');
  const ids = request.eligibleIds.map(BigInt);
  const first = ids[0];
  assert.ok(first >= 1n && first <= 150699n && (first - 1n) % 302n === 0n &&
    ids.every((id, i) => id === first + BigInt(i)),
  'release translation requires ascending contiguous IDs for one corpus note');
  const note = String((first - 1n) / 302n);
  const notePredicate = { op: 'eq', attributeId: 1, values: [{ id: 1, type: 'u64', value: note }] };
  const { eligibleIds, ...compatible } = request;
  return { ...compatible, filter: request.filter
    ? { op: 'and', children: [request.filter, notePredicate] } : notePredicate };
}
export function buildParityStore(implementation, root) {
  const store = implementation.openNamespace(root, 'notes', SPEC, { autoSealRows: 30200 });
  try {
    let notes = 0;
    for (const batch of documentBatches()) {
      store.upsert(batch);
      if (++notes % 100 === 0 && process.env.ZE_PARITY_PROGRESS) console.error(`built ${notes * 302} documents`);
    }
    store.seal();
    assert.equal(store.count().count, 151000n, 'complete parity corpus');
    return store;
  } catch (error) {
    store.close();
    throw error;
  }
}
function assertParityPopulation(store, relationshipIds) {
  assert.equal(store.count().count, 151000n, 'relationships must not add documents');
  assert.equal(relationshipIds.length, 500);
  assert.equal(new Set(relationshipIds).size, 500, '500 distinct relationships');
  const edges = store.graphGetRelationships(relationshipIds);
  assert.equal(edges.length, 500);
  for (const [note, edge] of edges.entries()) {
    assert.ok(edge, `relationship ${note} must exist`);
    assert.equal(edge.source, BigInt(302 * note + 301));
    assert.equal(edge.target, BigInt(302 * note + 302));
    assert.equal(edge.key, `parity-note-${note}`);
    assert.equal(edge.type, 'PARITY_NOTE');
  }
}
export function attachParityRelationships(store) {
  store.enableGraph();
  const relationshipIds = [];
  for (let note = 0; note < 500; note++) {
    const source = BigInt(302 * note + 301), target = source + 1n;
    const result = store.graphApply([{ kind: 'relationship', operation: 'create',
      namespace: 'parity', key: `parity-note-${note}`, revision: 1n,
      type: 'PARITY_NOTE', source, target }]);
    assert.equal(result.receipts.length, 1);
    assert.equal(result.receipts[0].kind, 'relationship');
    assert.equal(result.receipts[0].deleted, false);
    relationshipIds.push(result.receipts[0].id);
    if ((note + 1) % 100 === 0 && process.env.ZE_PARITY_PROGRESS) console.error(`attached ${note + 1} relationships`);
  }
  assertParityPopulation(store, relationshipIds);
  return relationshipIds;
}
export function folderEligibleIds(store, folders) {
  const generation = store.count().generation;
  const ids = [];
  // The fixture has 15100 documents per folder. Each singleton query remains
  // inside the shipped cap; exceeding any cap/budget throws, never truncates.
  for (const folder of new Set(folders.map(BigInt))) {
    const expected = store.count({ filter: { op: 'eq', attributeId: 2,
      values: [{ id: 2, type: 'u64', value: folder }] } });
    assert.equal(expected.generation, generation, 'one generation across folder partitions');
    const result = store.cypher(
      'MATCH (d:Document) WHERE d.folder IN $folders RETURN ze.node_id(d) AS id ORDER BY id',
      { folders: [folder] }, { maxRows: 65536 });
    assert.equal(result.admittedGeneration, generation, 'one generation across Cypher partitions');
    assert.equal(BigInt(result.rows.length), expected.count, 'Cypher must return the complete folder');
    assert.deepEqual(result.columns, ['id']);
    let previous;
    for (const row of result.rows) {
      assert.ok(row.length === 1 && typeof row[0] === 'string' && /^[0-9a-f]{32}$/.test(row[0]), 'complete u128 document ID row');
      const id = BigInt(`0x${row[0]}`);
      assert.ok(previous === undefined || previous < id, 'strictly ordered folder IDs');
      ids.push(id); previous = id;
    }
    if (process.env.ZE_PARITY_PROGRESS) console.error(`collected folder ${folder}: ${result.rows.length} IDs`);
  }
  assert.equal(store.count().generation, generation, 'folder union must use one generation');
  ids.sort((a, b) => a < b ? -1 : a > b ? 1 : 0);
  assert.equal(new Set(ids).size, ids.length, 'scalar folders form disjoint partitions');
  return ids;
}
const sha256 = bytes => createHash('sha256').update(bytes).digest('hex');
function segmentHashes(root, directory = root) {
  return readdirSync(directory, { withFileTypes: true }).sort((a, b) => a.name.localeCompare(b.name))
    .flatMap(entry => {
      const path = join(directory, entry.name);
      if (entry.isDirectory()) return segmentHashes(root, path);
      return entry.name.endsWith('.zseg') ? [[path.slice(root.length + 1), sha256(readFileSync(path))]] : [];
    });
}
export function replayUnified(implementation = createRequire(import.meta.url)('..'), queries) {
  const root = mkdtempSync(join(tmpdir(), 'ze-client-parity-unified-'));
  let store;
  try {
    store = buildParityStore(implementation, root);
    const graphFree = replayQueries(store, queries);
    const segments = segmentHashes(root);
    assert.equal(segments.length, 5, 'five immutable 30200-row segments');
    const relationshipIds = attachParityRelationships(store);
    assert.deepEqual(segmentHashes(root), segments, 'graph attachment preserves sealed segment bytes');
    const live = replayQueries(store, queries);
    compare(graphFree, live);
    store.close(); store = undefined;
    store = implementation.openNamespace(root, 'notes', SPEC, { readOnly: true });
    assertParityPopulation(store, relationshipIds);
    assert.deepEqual(segmentHashes(root), segments, 'reopen preserves sealed segment bytes');
    const reopened = replayQueries(store, queries);
    compare(live, reopened);
    return reopened;
  } finally {
    try { store?.close(); } finally { rmSync(root, { recursive: true, force: true }); }
  }
}
export function replay(implementation = createRequire(import.meta.url)('..'), queries) {
  const root = mkdtempSync(join(tmpdir(), 'ze-client-parity-'));
  let store;
  try {
    store = buildParityStore(implementation, root);
    store.close(); store = undefined;
    store = implementation.openNamespace(root, 'notes', SPEC, { readOnly: true });
    return replayQueries(store, queries);
  } finally {
    try { store?.close(); } finally { rmSync(root, { recursive: true, force: true }); }
  }
}
export function compare(expected, actual) {
  assert.equal(actual.results.length, expected.results.length, 'query count');
  const normalized = structuredClone(actual);
  for (const [index, result] of expected.results.entries()) {
    const observed = normalized.results[index];
    assert.equal(observed.hits.length, result.hits.length, `${result.name}: hit count`);
    for (const [rank, hit] of result.hits.entries()) {
      if (hit.fused !== null) {
        const score = observed.hits[rank].fused;
        assert.ok(Number.isFinite(score) && Number.isFinite(hit.fused) &&
          Math.abs(score - hit.fused) <= 1e-6, `${result.name}: rank ${rank} fused score`);
        observed.hits[rank].fused = hit.fused;
      }
    }
  }
  // Every remaining field, missing/extra field, query name and rank is exact.
  assert.deepEqual(normalized, expected);
}
if (process.argv[1] && import.meta.url === pathToFileURL(resolve(process.argv[1])).href) {
  const [command = 'check', ...args] = process.argv.slice(2);
  if (command === 'queries') {
    write('queries.json', { schema: 'zeppelin-client-query-shapes-v1', seed, provenance, queries: queryShapes() });
  } else if (command === 'compare') {
    assert.equal(args.length, 2, 'compare EXPECTED.json ACTUAL.json');
    compare(JSON.parse(readFileSync(args[0], 'utf8')), JSON.parse(readFileSync(args[1], 'utf8')));
    console.log('client parity: 500 queries match');
  } else if (command === 'release-reference') {
    assert.equal(args.length, 2, 'release-reference PACKAGE_DIR OUTPUT.json');
    const packageDirectory = realpathSync(resolve(args[0]));
    const manifest = JSON.parse(readFileSync(join(packageDirectory, 'package.json'), 'utf8'));
    assert.equal(manifest.name, '@zepdb/zeppelin-embed');
    assert.equal(manifest.version, '0.6.0');
    assert.ok(!readFileSync(join(packageDirectory, 'index.d.ts'), 'utf8').includes('eligibleIds'),
      'reference must be released 0.6.0 without eligibleIds; current checkout is not a release reference');
    const require = createRequire(import.meta.url);
    const implementation = require(packageDirectory);
    const translated = read('queries.json').queries.map(({ name, request }) =>
      ({ name, request: releaseCompatibleRequest(request) }));
    const actual = replay(implementation, translated);
    const addons = Object.keys(require.cache).filter(path =>
      path.startsWith(packageDirectory + sep) && path.endsWith('.node'));
    assert.equal(addons.length, 1, 'one loaded release addon');
    const receipt = {
      package: manifest.name, version: manifest.version,
      expectedSourceTag: 'v0.6.0', expectedSourceCommit: 'f39087d7b20016138d9a1946055bc229cdffdd23',
      platform: process.platform, arch: process.arch, node: process.version,
      addon: addons[0].slice(packageDirectory.length + 1), addonSha256: sha256(readFileSync(addons[0])),
      indexSha256: sha256(readFileSync(join(packageDirectory, 'index.js'))),
      queriesSha256: sha256(readFileSync(new URL('queries.json', fixtureDirectory))),
      generatorSha256: sha256(readFileSync(new URL('./client-parity-generator.mjs', import.meta.url))),
      translatedQueriesSha256: sha256(JSON.stringify(translated)),
      outputSha256: sha256(`${JSON.stringify(actual)}\n`),
      documents: 151000, relationships: 0, queries: actual.results.length, translatedQueries: 50,
    };
    writeFileSync(args[1], `${JSON.stringify(actual)}\n`);
    writeFileSync(`${args[1]}.provenance.json`, `${JSON.stringify(receipt, null, 2)}\n`);
    console.log(`client parity: release-reference completed (${actual.results.length} queries); provenance: ${args[1]}.provenance.json`);
  } else if (command === 'unified') {
    assert.equal(args.length, 2, 'unified PACKAGE_DIR EXPECTED.json');
    const reference = JSON.parse(readFileSync(args[1], 'utf8'));
    const implementation = createRequire(import.meta.url)(resolve(args[0]));
    const actual = replayUnified(implementation);
    compare(read('expected.json'), actual);
    compare(reference, actual);
    console.log(`client parity: unified matches both baselines (${actual.results.length} queries)`);
  } else {
    assert.ok(['record', 'check', 'replay'].includes(command), `unknown command ${command}`);
    const implementation = createRequire(import.meta.url)(args[0] ? resolve(args[0]) : '..');
    const actual = replay(implementation);
    if (command === 'record') write('expected.json', actual);
    else if (command === 'replay') {
      assert.ok(args[1], 'replay PACKAGE_PATH OUTPUT.json');
      writeFileSync(args[1], `${JSON.stringify(actual)}\n`);
    } else compare(read('expected.json'), actual);
    console.log(`client parity: ${command} completed (${actual.results.length} queries)`);
  }
}
