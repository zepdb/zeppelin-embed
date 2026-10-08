import test from 'node:test';
import { createHash } from 'node:crypto';
import assert from 'node:assert/strict';
import { percentile, beirMetrics, validateMeasurement, runFive, perfQueries,
  transcriptBatches, measureCell, preparePerfStore } from '../bench/unified-perf.mjs';

const digest = 'a'.repeat(64);
const datasets = ['trec-covid', 'fiqa', 'nfcorpus', 'scifact'];
function literalReceipt() {
  const queries = perfQueries();
  const artifact = { path: 'literal.jsonl', sha256: digest };
  const manifest = { schema: 'zeppelin-unified-perf-v1', seed: 386, dimensions: 8,
    attributes: ['note', 'folder', 'speaker', 'stream', 'startMs', 'endMs'].map((name, i) =>
      ({ id: i + 1, name, type: 'u64', nullable: false })),
    epoch: { modelId: 'zeppelin.vector-space', modelVersion: '1', normalization: 'none',
      runtime: 'cpuReference', computeUnits: 'cpu', maxTokens: 0, weightsDigest: [], alignmentDigest: [] },
    segmentBoundaries: [30000, 60000, 90000, 120000, 150000], rowCount: 150000,
    corpus: artifact, queryFile: artifact, queries,
    beir: datasets.map(name => ({ name, spec: { attributes: [] }, corpusCount: 2,
      queryCount: 1, excludedQueryCount: 1, queries: [{ id: 'q', text: 'literal', qrels: { d: 2 } }],
      inputs: { corpus: artifact, queries: artifact, qrels: artifact }, mapping: artifact, queryFile: artifact })) };
  return { schema: 'zeppelin-unified-perf-measurement-v1', manifest, workloadSha256: createHash('sha256').update(JSON.stringify(manifest, (key, value) => key === 'path' ? undefined : value && typeof value === 'object' && !Array.isArray(value) ? Object.fromEntries(Object.keys(value).sort().map(k => [k, value[k]])) : value)).digest('hex'),
    provenance: { source: 'npm', name: '@zepdb/zeppelin-embed', version: '0.6.0',
      tag: 'v0.6.0', commit: 'f39087d7b20016138d9a1946055bc229cdffdd23',
      packageSha256: digest, addonSha256: digest, entrySha256: digest,
      addonPath: '/literal/addon.node', releaseFlags: 'published npm prebuild; flags unavailable' },
    host: { cpu: 'literal', cores: 1, ramBytes: 1024, os: 'literal', arch: 'arm64',
      platform: 'darwin', node: 'v22', rust: 'rustc literal' },
    settings: { warmups: 20, samples: 1000, threadBudget: 1 }, root: '/literal/store',
    repetitions: Array.from({ length: 5 }, (_, i) => ({ pid: 100 + i, tainted: false,
      before: { load: [0, 0, 0], ps: 'quiet', taints: [] }, after: { load: [0, 0, 0], ps: 'quiet', taints: [] },
      cells: queries.map(({ name }) => ({ name, samplesMs: Array(1000).fill(1),
        p50: 1, p95: 1, p99: 1, hits: [] })),
      typeahead: { p50: 1, p95: 1, p99: 1 },
      beir: datasets.map(name => ({ name, queryCount: 1, excludedQueryCount: 1,
        results: [{ id: 'q', rankedIds: ['d'], scoreBits: ['3ff0000000000000'],
          recallAt100: 1, ndcgAt10: 1 }], recallAt100: 1, ndcgAt10: 1 })) })) };
}

test('unified_perf::percentiles_use_nearest_rank_and_reject_empty_or_nonfinite_samples', () => {
  assert.equal(percentile([4, 1, 3, 2], .5), 2);
  assert.equal(percentile([4, 1, 3, 2], .95), 4);
  assert.equal(percentile([0], 1), 0);
  for (const values of [[], [NaN], [Infinity], [-1]]) assert.throws(() => percentile(values, .5));
  for (const p of [0, -1, 1.1, NaN]) assert.throws(() => percentile([1], p));
});

test('unified_perf::beir_metrics_match_literal_graded_and_empty_results', () => {
  assert.deepEqual(beirMetrics(['a', 'b'], { a: 2, b: 1 }), { recallAt100: 1, ndcgAt10: 1 });
  assert.deepEqual(beirMetrics([], { a: 2, b: 1 }), { recallAt100: 0, ndcgAt10: 0 });
  const one = beirMetrics(['a'], { a: 2, b: 1 });
  assert.equal(one.recallAt100, .5);
  assert.ok(Math.abs(one.ndcgAt10 - 0.7601875334318685) < 1e-15);
  assert.throws(() => beirMetrics(['a', 'a'], { a: 1 }));
  assert.throws(() => beirMetrics([], { a: 0 }));
  assert.throws(() => beirMetrics([], { a: -1 }));
});

test('unified_perf::perf_fixture_is_exactly_150000_rows_and_five_segments', () => {
  // Literal meeting: the two final rows must never enter the performance fixture.
  const row = { id: 1n };
  const batches = [...transcriptBatches([Array(302).fill(row)])];
  assert.equal(batches.length, 1); assert.equal(batches[0].length, 300);
  assert.strictEqual(batches[0][0], row);
  let rows = 0, closed = false;
  const seals = [];
  preparePerfStore({ openNamespace(root, name, spec, options) {
    assert.ok(!Object.hasOwn(options, 'autoSealRows'), 'disabled by omission; zero is rejected by the package');
    return { upsert(batch) { assert.strictEqual(batch[0], row); rows += batch.length; },
      seal() { seals.push(rows); }, close() { closed = true; } };
  } }, '/literal', () => {}, Array(500).fill(Array(302).fill(row)));
  assert.equal(rows, 150000); assert.deepEqual(seals, [30000, 60000, 90000, 120000, 150000]);
  assert.equal(closed, true);
  const value = literalReceipt();
  assert.deepEqual(value.manifest.segmentBoundaries, [30000, 60000, 90000, 120000, 150000]);
  assert.doesNotThrow(() => validateMeasurement(value));
  value.manifest.rowCount = 151000;
  assert.throws(() => validateMeasurement(value));
});

test('unified_perf::baseline_requires_five_complete_fresh_processes', () => {
  const value = literalReceipt();
  let launched = 0;
  assert.equal(runFive({ worker: () => value.repetitions[launched++] }).length, 5);
  assert.equal(launched, 5);
  assert.doesNotThrow(() => validateMeasurement(value));
  const mutations = [v => v.repetitions.pop(), v => v.repetitions[1].pid = 100,
    v => v.repetitions[0].cells.pop(), v => v.repetitions[0].beir[0].results.pop(),
    v => delete v.provenance.addonSha256, v => v.repetitions[0].tainted = true,
    v => v.repetitions[0].cells[0].samplesMs[0] = 0,
    v => v.manifest.beir[0].inputs.qrels.sha256 = 'missing',
    v => v.repetitions[0].beir[0].results[0].scoreBits[0] = '7ff0000000000000',
    v => v.repetitions[0].beir[0].results[0].ndcgAt10 = .9,
    v => v.provenance.version = '0.7.0', v => v.settings.samples = 999,
    v => v.repetitions[0].typeahead.p95 = 2, v => v.unexpected = true];
  for (const mutate of mutations) {
    const copy = structuredClone(value); mutate(copy);
    assert.throws(() => validateMeasurement(copy));
  }
});

test('unified_perf::measure_cell_times_only_complete_synchronous_queries', () => {
  let calls = 0;
  const store = { query(request) { assert.equal(request.k, 10); calls++;
    return { hits: [{ id: 1n, score: 2 }], approximate: false, mode: 'lexical', exactRescore: false, budgetExhausted: false }; } };
  const result = measureCell(store, { k: 10 }, 2, 3);
  assert.equal(calls, 5); assert.equal(result.samplesMs.length, 3);
  assert.ok(result.samplesMs.every(x => x > 0));
  assert.deepEqual(result.hits, [{ id: '1', scoreBits: '4000000000000000' }]);
  for (const bad of [{ budgetExhausted: true }, { approximate: true }, { mode: 'hybrid' },
    { hits: [{ id: 1n, score: Infinity }] }, { hits: [{ id: 1n, score: 1 }, { id: 1n, score: 1 }] }]) {
    assert.throws(() => measureCell({ query: () => ({ hits: [], approximate: false,
      mode: 'lexical', exactRescore: false, budgetExhausted: false, ...bad }) }, { k: 10 }, 0, 1));
  }
});

test('unified_perf::hybrid_measurement_requires_exact_vector_scores', () => {
  assert.throws(() => measureCell({ query: () => ({ mode: 'hybrid', hits: [], approximate: false,
    exactRescore: false, budgetExhausted: false }) }, { k: 10, vector: [0, 0], tier: 'exact' }, 0, 1));
});
