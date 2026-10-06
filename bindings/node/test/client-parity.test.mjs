// Synthetic corpus generated from ADR-017 assumptions, NOT the client's recorded queries:
// replace or extend with the client's real corpus when provided.
import assert from 'node:assert/strict';
import { readFileSync } from 'node:fs';
import test from 'node:test';
import { replay, compare, fixtureDirectory } from '../bench/client-parity.mjs';

const expected = () => JSON.parse(readFileSync(new URL('expected.json', fixtureDirectory), 'utf8'));
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
