// Synthetic corpus generated from ADR-017 assumptions, NOT the client's recorded queries:
// replace or extend with the client's real corpus when provided.
import assert from 'node:assert/strict';
import { mkdtempSync, readFileSync, rmSync, writeFileSync } from 'node:fs';
import { tmpdir } from 'node:os';
import { join, resolve } from 'node:path';
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
export function replay(implementation = createRequire(import.meta.url)('..'), queries) {
  const root = mkdtempSync(join(tmpdir(), 'ze-client-parity-'));
  let store;
  try {
    store = implementation.openNamespace(root, 'notes', SPEC, { autoSealRows: 30200 });
    let notes = 0;
    for (const batch of documentBatches()) {
      store.upsert(batch);
      if (++notes % 100 === 0 && process.env.ZE_PARITY_PROGRESS) console.error(`built ${notes * 302} documents`);
    }
    store.seal();
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
