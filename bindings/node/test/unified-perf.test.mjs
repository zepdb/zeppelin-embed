import test from 'node:test';
import * as gate from '../bench/unified-perf.mjs';
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
      cells: queries.map(({ name }) => ({ name, samplesMs: Array(name.startsWith('cypher-') ? 10 : 1000).fill(1),
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

// Independent receipts: no measured engine output is used as the oracle.
function comparatorFixture() {
  const baseline = literalReceipt(), candidate = literalReceipt();
  candidate.provenance = { ...candidate.provenance, source: 'checkout', tag: null,
    commit: 'b'.repeat(40), addonSha256: 'b'.repeat(64), releaseFlags: 'npm run build:native (release)' };
  candidate.candidateOnlyQueries = ['lexical', 'hybrid'].flatMap(kind => {
    const request = { ...baseline.manifest.queries.find(q => q.name === `${kind}-unfiltered`).request,
      eligibleIds: Array.from({ length: 10000 }, (_, i) => String(Math.floor(i / 300) * 302 + i % 300 + 1)) };
    return [{ name: `${kind}-eligible`, request }];
  });
  candidate.candidateOnlyQueries.push(...candidate.candidateOnlyQueries.map(q => ({ ...q, name: `cypher-${q.name}` })));
  const extras = ['lexical-eligible', 'hybrid-eligible', 'cypher-lexical-eligible', 'cypher-hybrid-eligible'];
  for (const r of candidate.repetitions) for (const name of extras)
    r.cells.push({ name, samplesMs: Array(name.startsWith('cypher-') ? 10 : 1000).fill(2), p50: 2, p95: 2, p99: 2, hits: [] });
  function counters(receipt, unified) {
    const names = receipt.manifest.queries.map(q => q.name);
    if (unified) names.push(...extras.slice(0, 2));
    return { schema: 'zeppelin-unified-perf-counters-v1', workloadSha256: receipt.workloadSha256,
      manifestSha256: digest, revision: receipt.provenance.commit, binarySha256: unified ? 'b'.repeat(64) : digest,
      optLevel: '3', warmups: 20, samples: 5, threadBudget: 1,
      ...(unified ? { acceptance: true, unified: true, rowCount: 150000,
        graph: { documents: 150000, relationships: 500 }, candidateOnlyQueries: candidate.candidateOnlyQueries.slice(0, 2),
        eligibilityAccounting: 'query latency includes construction; work counters omit eligibility-map construction' } : {}),
      queries: names.map(name => ({ name, hits: [], descriptive: Array(5).fill({ plan: 'literal' }),
        samples: Array.from({ length: 5 }, () => ({ lexical: { docs_evaluated: 10,
          postings_decoded: 10, blocks_decoded: 0 }, scan: { dims_touched: 0, bytes_read: 0 },
          graph: { candidates_scored: 0, candidates_rescored: 0 },
          hybrid: name.startsWith('hybrid') ? { lexical_full_materializations: 0, vector_candidates_produced: 10,
            lexical_candidates_produced: 10, total_cross_filled_vector: 0, total_cross_filled_lexical: 0 } : null,
          fusion: name.startsWith('hybrid') ? { rounds: 1 } : null })) })) };
  }
  return [baseline, candidate, counters(baseline, false), counters(candidate, true)];
}
function accepts(f) { assert.doesNotThrow(() => gate.comparePerf(...f)); }
function rejects(f) { assert.throws(() => gate.comparePerf(...f)); }
function cost(f, name, group, key, value) {
  for (const s of f[3].queries.find(q => q.name === name).samples) s[group][key] = value;
}
function latency(f, name, value, repetition) {
  for (const r of repetition === undefined ? f[1].repetitions : [f[1].repetitions[repetition]]) {
    const c = r.cells.find(c => c.name === name);
    c.samplesMs.fill(value); c.p50 = value; c.p95 = value; c.p99 = value;
    const samples = r.cells.slice(0, 100).flatMap(c => c.samplesMs);
    r.typeahead = { p50: percentile(samples, .5), p95: percentile(samples, .95), p99: percentile(samples, .99) };
  }
}
test('unified_perf::comparator_rejects_counter_regression_and_new_work_from_zero', () => {
  const f = comparatorFixture(); cost(f, 'lexical-unfiltered', 'lexical', 'docs_evaluated', 11); accepts(f);
  cost(f, 'lexical-unfiltered', 'lexical', 'docs_evaluated', 12); rejects(f);
  const z = comparatorFixture(); cost(z, 'lexical-unfiltered', 'scan', 'dims_touched', 1); rejects(z);
  const e = comparatorFixture(); cost(e, 'lexical-eligible', 'lexical', 'docs_evaluated', 20); accepts(e);
  cost(e, 'lexical-eligible', 'lexical', 'docs_evaluated', 21); rejects(e);
  const nondeterministic = comparatorFixture(); nondeterministic[3].queries[0].samples[1].lexical.docs_evaluated++;
  rejects(nondeterministic);
});
test('unified_perf::comparator_rejects_missing_cells_and_wrong_provenance', () => {
  accepts(comparatorFixture());
  for (const mutate of [f => f[1].repetitions[0].cells.pop(), f => f[3].queries.pop(),
    f => f[1].provenance.source = 'npm', f => f[3].revision = 'c'.repeat(40),
    f => f[0].provenance.commit = 'b'.repeat(40), f => f[1].host.cpu = 'other',
    f => f[1].manifest.corpus.sha256 = 'c'.repeat(64), f => f[3].acceptance = false,
    f => f[3].optLevel = '0', f => f[1].candidateOnlyQueries[0].request.eligibleIds[0] = '0',
    f => f[1].provenance.addonSha256 = digest, f => f[3].binarySha256 = digest, f => f[1].repetitions[0].cells[0].samplesMs.pop(),
    f => f[1].repetitions[0].cells[0].samplesMs[0] = Infinity,
    f => f[1].repetitions[0].tainted = true]) {
    const f = comparatorFixture(); mutate(f); rejects(f);
  }
});
test('unified_perf::comparator_rejects_beir_rank_score_or_metric_bit_drift', () => {
  accepts(comparatorFixture());
  for (const mutate of [q => q.rankedIds[0] = 'other', q => q.scoreBits[0] = '3ff0000000000001',
    q => q.recallAt100 = 1 - Number.EPSILON, q => q.ndcgAt10 = 1 - Number.EPSILON,
    q => q.scoreBits[0] = '7ff0000000000000']) {
    const f = comparatorFixture(); for (const r of f[1].repetitions) mutate(r.beir[0].results[0]); rejects(f);
  }
  const missing = comparatorFixture(); missing[1].repetitions[0].beir[0].results.pop(); rejects(missing);
  const mean = comparatorFixture(); mean[1].repetitions[0].beir[0].ndcgAt10 -= Number.EPSILON; rejects(mean);
});
test('unified_perf::comparator_enforces_typeahead_150k_and_eligible_limits', () => {
  const t = comparatorFixture();
  for (const c of t[1].repetitions[0].cells.slice(0, 100)) latency(t, c.name, 30, 0);
  accepts(t); latency(t, t[1].repetitions[0].cells[0].name, 30.000001, 0); rejects(t);
  for (const name of ['lexical-unfiltered', 'hybrid-unfiltered', 'lexical-folder-0', 'hybrid-folder-0']) {
    const f = comparatorFixture(); latency(f, name, 1.1); accepts(f);
    latency(f, name, 1.100001); rejects(f);
  }
  const e = comparatorFixture(); latency(e, 'lexical-eligible', 2); accepts(e);
  latency(e, 'lexical-eligible', 2.000001); rejects(e);
  const p = comparatorFixture();
  for (const r of p[1].repetitions) { const c = r.cells.find(c => c.name === 'lexical-unfiltered');
    c.samplesMs = [...Array(949).fill(1), ...Array(51).fill(1.100001)]; c.p50 = 1; c.p95 = 1.100001; c.p99 = 1.100001; }
  rejects(p);
});

test('unified_perf::unified_preparation_is_relationship_only_and_checks_reopen', () => {
  let rows = 0, enabled = false, opens = 0, closes = 0;
  const edges = new Map(), seals = [], manifest = {};
  const implementation = { openNamespace(root, name, spec, options) {
    opens++;
    return { upsert(batch) { assert.equal(enabled, false); rows += batch.length; },
      seal() { seals.push(rows); }, close() { closes++; }, count() { return { count: BigInt(rows) }; },
      enableGraph() { enabled = true; },
      graphApply(items) { assert.equal(items.length, 1); assert.equal(enabled, true);
        const item = items[0]; assert.equal(item.kind, 'relationship'); assert.equal(item.operation, 'create');
        assert.equal(item.source, BigInt(302 * edges.size + 1)); assert.equal(item.target, item.source + 1n);
        assert.equal(item.key, `perf-${edges.size}`); assert.equal(item.revision, 1n);
        const id = BigInt(edges.size + 1); edges.set(id, item);
        return { receipts: [{ kind: 'relationship', deleted: false, id }] }; },
      graphGetRelationships(ids) { assert.equal(enabled, true); return ids.map(id => edges.get(id)); } };
  } };
  gate.prepareUnifiedPerfStore(implementation, '/literal', manifest, () => {}, Array(500).fill(Array(302).fill({ id: 1n })));
  assert.equal(rows, 150000); assert.equal(edges.size, 500); assert.equal(opens, 3); assert.equal(closes, 3);
  assert.equal(manifest.perfRelationships.length, 500);
  assert.deepEqual(seals, [30000, 60000, 90000, 120000, 150000]);
  const declared = gate.unifiedQueries();
  assert.equal(declared[0].request.eligibleIds.length, 10000);
  assert.equal(declared[0].request.eligibleIds[300], '303');
  assert.equal(declared[0].request.eligibleIds.at(-1), '10066');
  assert.deepEqual(declared[0].request.eligibleIds, declared[2].request.eligibleIds);
});

test('unified_perf::cypher_cells_use_live_prefix_and_normalize_u128_id_text', () => {
  const request = gate.unifiedQueries(true)[0].request;
  let calls = 0;
  const store = { cypher(text, params) {
    calls++; assert.match(text, /collect\(DISTINCT d\)/); assert.match(text, /ze\.text_search/);
    assert.match(text, /ze\.node_id\(d\) <= \$lastId/);
    assert.equal(params.lastId, '00000000000000000000000000000064');
    return { columns: ['id', 'score'], receipts: [], rows: [['00000000000000000000000000000001', 1]] };
  } };
  const cell = measureCell(store, request, 1, 2, true);
  assert.equal(calls, 3); assert.deepEqual(cell.hits, [{ id: '1', scoreBits: '3ff0000000000000' }]);
  for (const bad of [ [['not-hex', 1]], [['00000000000000000000000000000001', Infinity]],
    [['00000000000000000000000000000001', 1, 'extra']]]) {
    assert.throws(() => measureCell({ cypher: () => ({ columns: ['id', 'score'], receipts: [], rows: bad }) }, request, 0, 1, true));
  }
});
