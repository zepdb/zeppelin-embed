// Explicit, owner-run synthetic performance baseline. Importing never opens a store.
import assert from 'node:assert/strict';
import { createHash } from 'node:crypto';
import { createReadStream, existsSync, mkdirSync, openSync, closeSync, readFileSync, writeFileSync, writeSync } from 'node:fs';
import { cpus, totalmem, platform, arch, release, loadavg } from 'node:os';
import { dirname, join, resolve } from 'node:path';
import { createRequire } from 'node:module';
import { createInterface } from 'node:readline';
import { spawnSync } from 'node:child_process';
import { pathToFileURL, fileURLToPath } from 'node:url';
import { SPEC, seed, dimensions, documentBatches, queryShapes, decodeRequest } from './client-parity-generator.mjs';

const DATASETS = ['trec-covid', 'fiqa', 'nfcorpus', 'scifact'];
const BOUNDARIES = [30000, 60000, 90000, 120000, 150000];
const SETTINGS = { warmups: 20, samples: 1000, threadBudget: 1 };
const TAG = 'v0.6.0', COMMIT = 'f39087d7b20016138d9a1946055bc229cdffdd23';
const EPOCH = { modelId: 'zeppelin.vector-space', modelVersion: '1', normalization: 'none',
  runtime: 'cpuReference', computeUnits: 'cpu', maxTokens: 0, weightsDigest: [], alignmentDigest: [] };
const json = value => `${JSON.stringify(value)}\n`;
const read = path => JSON.parse(readFileSync(path, 'utf8'));
const sha = bytes => createHash('sha256').update(bytes).digest('hex');
// Only logical inputs enter this hash; artifact locations and implementation do not.
const workloadHash = manifest => sha(JSON.stringify(manifest, (key, value) => {
  if (key === 'path') return undefined;
  if (value && typeof value === 'object' && !Array.isArray(value))
    return Object.fromEntries(Object.keys(value).sort().map(k => [k, value[k]]));
  return value;
}));
const hashPattern = /^[a-f0-9]{64}$/;
function keys(value, expected) { assert.deepEqual(Object.keys(value).sort(), expected.split(' ').sort(), 'schema fields'); }
function unique(values) { assert.equal(new Set(values).size, values.length, 'duplicate identity'); }
function positive(value) { assert.ok(Number.isFinite(value) && value > 0, 'positive finite value required'); }
function bits(value) {
  assert.ok(Number.isFinite(value), 'nonfinite score');
  const bytes = Buffer.alloc(8); bytes.writeDoubleBE(value); return bytes.toString('hex');
}
function numberFromBits(value) {
  assert.match(value, /^[a-f0-9]{16}$/); const number = Buffer.from(value, 'hex').readDoubleBE();
  assert.ok(Number.isFinite(number), 'nonfinite score bits'); return number;
}
function freshJSON(path, value) { mkdirSync(dirname(path), { recursive: true }); writeFileSync(path, json(value), { flag: 'wx' }); }
async function fileHash(path) {
  const hash = createHash('sha256'); for await (const chunk of createReadStream(path)) hash.update(chunk);
  return hash.digest('hex');
}
async function artifact(path) { return { path: resolve(path), sha256: await fileHash(path) }; }
async function* lines(path) {
  const reader = createInterface({ input: createReadStream(path), crlfDelay: Infinity });
  for await (const line of reader) { if (line.trim()) yield line; }
}

export function percentile(samples, fraction) {
  assert.ok(samples.length && Number.isFinite(fraction) && fraction > 0 && fraction <= 1);
  assert.ok(samples.every(value => Number.isFinite(value) && value >= 0));
  const sorted = [...samples].sort((a, b) => a - b);
  return sorted[Math.ceil(fraction * sorted.length) - 1];
}
function percentiles(samples) { return { p50: percentile(samples, .5), p95: percentile(samples, .95), p99: percentile(samples, .99) }; }
export function beirMetrics(rankedIds, qrels) {
  unique(rankedIds);
  const grades = Object.values(qrels);
  assert.ok(grades.every(g => Number.isSafeInteger(g) && g >= 0 && g <= 0xffffffff));
  const ideal = grades.filter(g => g > 0).sort((a, b) => b - a);
  assert.ok(ideal.length, 'no positive judgement: exclude this query');
  const dcg = values => values.slice(0, 10).reduce((sum, g, i) => sum + g / Math.log2(i + 2), 0);
  return { recallAt100: rankedIds.slice(0, 100).filter(id => (Object.hasOwn(qrels, id) ? qrels[id] : 0) > 0).length / ideal.length,
    ndcgAt10: dcg(rankedIds.map(id => Object.hasOwn(qrels, id) ? qrels[id] : 0)) / dcg(ideal) };
}
export function* transcriptBatches(batches = documentBatches()) {
  for (const batch of batches) { assert.equal(batch.length, 302); yield batch.slice(0, 300); }
}
export function perfQueries() {
  const shapes = queryShapes();
  const vector = shapes.find(q => q.request.vector).request.vector;
  const filter = { op: 'eq', attributeId: 2, values: [{ id: 2, type: 'u64', value: '0' }] };
  return [...shapes.slice(100, 200).map(({ name, request }) => ({ name,
    request: { text: request.text, lastAsPrefix: true, k: 10, threadBudget: 1 } })),
    ...[false, true].flatMap(filtered => [false, true].map(hybrid => ({
      name: `${hybrid ? 'hybrid' : 'lexical'}-${filtered ? 'folder-0' : 'unfiltered'}`,
      request: { text: 'harbour', k: 10, threadBudget: 1,
        ...(hybrid ? { vector, tier: 'exact', alpha: .5 } : {}), ...(filtered ? { filter } : {}) } })))];
}
export function preparePerfStore(implementation, directory, onBatch = () => {}, batches = documentBatches()) {
  const store = implementation.openNamespace(directory, 'perf', SPEC, { autoMerge: false });
  let rows = 0;
  try {
    for (const batch of transcriptBatches(batches)) {
      onBatch(batch); store.upsert(batch); rows += batch.length;
      if (BOUNDARIES.includes(rows)) store.seal();
    }
    assert.equal(rows, 150000);
  } finally { store.close(); }
}
function recordResult(result, request) {
  const k = request.k;
  assert.equal(result.mode, request.vector ? 'hybrid' : 'lexical', 'wrong query mode');
  assert.equal(result.budgetExhausted, false, 'budget exhausted');
  assert.equal(result.approximate, false, 'approximate results');
  if (request.vector) assert.equal(result.exactRescore, true, 'inexact vector scores');
  assert.ok(Array.isArray(result.hits) && result.hits.length <= k);
  const hits = result.hits.map(hit => ({ id: String(hit.id), scoreBits: bits(hit.score) }));
  unique(hits.map(hit => hit.id)); return hits;
}
export function measureCell(store, request, warmups = 20, samples = 1000) {
  assert.ok(Number.isInteger(warmups) && warmups >= 0 && Number.isInteger(samples) && samples > 0);
  const decoded = decodeRequest(request); // decoding/allocation is outside the timer
  for (let i = 0; i < warmups; i++) recordResult(store.query(decoded), request);
  const samplesMs = []; let hits;
  for (let i = 0; i < samples; i++) {
    const start = process.hrtime.bigint();
    const result = store.query(decoded);
    const elapsed = process.hrtime.bigint() - start;
    const sample = Number(elapsed) / 1e6; positive(sample); samplesMs.push(sample);
    const observed = recordResult(result, request);
    if (hits) assert.deepEqual(observed, hits, 'nondeterministic hits');
    hits = observed;
  }
  return { samplesMs, hits };
}

function loadPackage(path) {
  path = resolve(path); const require = createRequire(import.meta.url);
  const metadata = read(join(path, 'package.json'));
  assert.equal(metadata.name, '@zepdb/zeppelin-embed'); assert.equal(metadata.version, '0.6.0');
  if (metadata.gitHead) assert.equal(metadata.gitHead, COMMIT);
  const implementation = require(path);
  const addons = Object.keys(require.cache).filter(p => p.startsWith(`${path}/`) && p.endsWith('.node'));
  assert.equal(addons.length, 1, 'identify the addon actually loaded');
  return { implementation, provenance: { source: 'npm', name: metadata.name, version: metadata.version,
    tag: TAG, commit: COMMIT, packageSha256: sha(readFileSync(join(path, 'package.json'))),
    entrySha256: sha(readFileSync(require.resolve(path))), addonSha256: sha(readFileSync(addons[0])),
    addonPath: addons[0], releaseFlags: 'published npm prebuild; flags unavailable' } };
}
export function parseBeirQrels(text) {
  const qrels = Object.create(null);
  const rows = text.trim().split(/\r?\n/);
  for (const [i, line] of rows.entries()) {
    if (i === 0 && line === 'query-id\tcorpus-id\tscore') continue;
    const parts = line.split('\t'); assert.equal(parts.length, 3, 'malformed qrels');
    const [query, document, grade] = parts;
    assert.ok(query && document && /^-?\d+$/.test(grade), 'malformed qrels');
    const value = Number(grade); assert.ok(Number.isSafeInteger(value) && value >= -1 && value <= 0xffffffff);
    qrels[query] ??= Object.create(null);
    assert.ok(!Object.hasOwn(qrels[query], document), 'duplicate qrel');
    // TREC -1 means explicitly nonrelevant, matching the existing BEIR loader.
    qrels[query][document] = Math.max(0, value);
  }
  return qrels;
}
async function prepareBeir(implementation, root, directory, name, output) {
  const base = join(directory, name);
  const inputs = { corpus: await artifact(join(base, 'corpus.jsonl')),
    queries: await artifact(join(base, 'queries.jsonl')), qrels: await artifact(join(base, 'qrels/test.tsv')) };
  const corpus = [];
  for await (const line of lines(inputs.corpus.path)) {
    const row = JSON.parse(line); assert.ok(typeof row._id === 'string' && row._id.length);
    assert.ok(typeof row.title === 'string' && typeof row.text === 'string'); corpus.push(row);
  }
  corpus.sort((a, b) => a._id < b._id ? -1 : a._id > b._id ? 1 : 0);
  const mapping = corpus.map(row => row._id); unique(mapping); positive(mapping.length);
  const ids = new Set(mapping), queryIds = new Set(), queries = [];
  const qrels = parseBeirQrels(readFileSync(inputs.qrels.path, 'utf8')); let excludedQueryCount = 0;
  for await (const line of lines(inputs.queries.path)) {
    const row = JSON.parse(line);
    assert.ok(typeof row._id === 'string' && row._id.length && typeof row.text === 'string');
    assert.ok(!queryIds.has(row._id), 'duplicate query'); queryIds.add(row._id);
    const judgements = qrels[row._id];
    if (!judgements || !Object.values(judgements).some(g => g > 0)) { excludedQueryCount++; continue; }
    queries.push({ id: row._id, text: row.text, qrels: judgements });
  }
  for (const [id, judgements] of Object.entries(qrels)) {
    assert.ok(queryIds.has(id), 'qrel names missing query');
    for (const id of Object.keys(judgements)) assert.ok(ids.has(id), 'qrel names missing document');
  }
  queries.sort((a, b) => a.id < b.id ? -1 : a.id > b.id ? 1 : 0); positive(queries.length);
  const spec = { attributes: [] };
  const store = implementation.openNamespace(root, name, spec, { autoMerge: false });
  try {
    for (let first = 0; first < corpus.length; first += 1000) {
      store.upsert(corpus.slice(first, first + 1000).map((row, i) => ({ id: BigInt(first + i + 1),
        revision: 1n, text: `${row.title}\n${row.text}` })));
      if ((first + 1000) % 30000 === 0) store.seal();
    }
    store.seal();
  } finally { store.close(); }
  const mappingPath = join(output, `${name}-mapping.json`), queryPath = join(output, `${name}-queries.json`);
  freshJSON(mappingPath, mapping); freshJSON(queryPath, queries);
  return { name, spec, corpusCount: corpus.length, queryCount: queries.length, excludedQueryCount,
    inputs, mapping: await artifact(mappingPath), queryFile: await artifact(queryPath), queries };
}
async function prepare(config) {
  assert.ok(!existsSync(config.root), 'root already exists; use a fresh root');
  assert.ok(!existsSync(config.out), 'manifest already exists');
  const { implementation, provenance } = loadPackage(config.package);
  mkdirSync(dirname(config.out), { recursive: true }); mkdirSync(config.root);
  const corpusPath = join(dirname(config.out), 'perf-rows.jsonl');
  const fd = openSync(corpusPath, 'wx');
  try { preparePerfStore(implementation, config.root, batch => {
    for (const row of batch) writeSync(fd, json({ ...row, id: String(row.id), revision: String(row.revision),
      timestamp: String(row.timestamp), vector: Array.from(row.vector),
      attributes: row.attributes.map(a => ({ ...a, value: String(a.value) })) }));
  }); } finally { closeSync(fd); }
  const queries = perfQueries(), queryPath = join(dirname(config.out), 'perf-queries.json');
  freshJSON(queryPath, queries);
  const beir = [];
  for (const name of DATASETS) beir.push(await prepareBeir(implementation, config.root, config.beir, name, dirname(config.out)));
  const manifest = { schema: 'zeppelin-unified-perf-v1', seed, dimensions, attributes: SPEC.attributes,
    epoch: EPOCH, rowCount: 150000, segmentBoundaries: BOUNDARIES,
    corpus: await artifact(corpusPath), queryFile: await artifact(queryPath), queries, beir };
  freshJSON(config.out, manifest);
  freshJSON(join(config.root, 'prepared.json'), { workloadSha256: workloadHash(manifest), provenance });
}
function validateManifest(m) {
  keys(m, 'schema seed dimensions attributes epoch rowCount segmentBoundaries corpus queryFile queries beir');
  assert.equal(m.schema, 'zeppelin-unified-perf-v1'); assert.equal(m.seed, seed); assert.equal(m.dimensions, dimensions);
  assert.deepEqual(m.attributes, SPEC.attributes); assert.deepEqual(m.epoch, EPOCH);
  assert.equal(m.rowCount, 150000); assert.deepEqual(m.segmentBoundaries, BOUNDARIES);
  assert.deepEqual(m.queries, perfQueries());
  function checkArtifact(a) { keys(a, 'path sha256'); assert.ok(typeof a.path === 'string' && a.path.length); assert.match(a.sha256, hashPattern); }
  checkArtifact(m.corpus); checkArtifact(m.queryFile);
  assert.deepEqual(m.beir.map(d => d.name), DATASETS);
  for (const d of m.beir) {
    keys(d, 'name spec corpusCount queryCount excludedQueryCount inputs mapping queryFile queries');
    assert.deepEqual(d.spec, { attributes: [] }); positive(d.corpusCount);
    assert.ok(Number.isInteger(d.excludedQueryCount) && d.excludedQueryCount >= 0);
    keys(d.inputs, 'corpus queries qrels'); Object.values(d.inputs).forEach(checkArtifact);
    checkArtifact(d.mapping); checkArtifact(d.queryFile); assert.equal(d.queryCount, d.queries.length); positive(d.queryCount);
    const ids = d.queries.map(q => q.id); unique(ids); assert.deepEqual(ids, [...ids].sort());
    for (const q of d.queries) { keys(q, 'id text qrels'); assert.ok(q.id && typeof q.text === 'string'); beirMetrics([], q.qrels); }
  }
}
async function verifyArtifacts(manifest) {
  validateManifest(manifest);
  const artifacts = [manifest.corpus, manifest.queryFile, ...manifest.beir.flatMap(d =>
    [d.mapping, d.queryFile, ...Object.values(d.inputs)])];
  for (const a of artifacts) assert.equal(await fileHash(a.path), a.sha256, `changed input ${a.path}`);
  assert.deepEqual(read(manifest.queryFile.path), manifest.queries);
  for (const d of manifest.beir) assert.deepEqual(read(d.queryFile.path), d.queries);
}
function snapshot(parentPid) {
  const ps = spawnSync('ps', ['-axo', 'pid=,ppid=,pcpu=,comm='], { encoding: 'utf8' });
  assert.equal(ps.status, 0, 'ps snapshot failed');
  const taints = [];
  for (const line of ps.stdout.split('\n')) {
    const match = line.trim().match(/^(\d+)\s+(\d+)\s+([\d.]+)\s+(.+)$/);
    if (!match || [process.pid, parentPid].includes(Number(match[1]))) continue;
    if (Number(match[3]) >= 20 || /(?:^|\/)(?:cargo|rustc|clang|cc1|ninja|make|ffmpeg)$/.test(match[4])) taints.push(line.trim());
  }
  const load = loadavg(); if (load[0] > 3) taints.push('load1 > 3');
  return { load, ps: ps.stdout, taints };
}
function measureBeir(implementation, root, dataset) {
  const mapping = read(dataset.mapping.path); assert.equal(mapping.length, dataset.corpusCount); unique(mapping);
  const store = implementation.openNamespace(root, dataset.name, dataset.spec, { readOnly: true });
  try {
    store.warmLexical();
    const results = dataset.queries.map(q => {
      const request = { text: q.text, k: 100, threadBudget: 1 };
      const hits = recordResult(store.query(request), request);
      const rankedIds = hits.map(hit => { const id = BigInt(hit.id);
        assert.ok(id > 0n && id <= BigInt(mapping.length)); return mapping[Number(id - 1n)]; });
      return { id: q.id, rankedIds, scoreBits: hits.map(h => h.scoreBits), ...beirMetrics(rankedIds, q.qrels) };
    });
    return { name: dataset.name, queryCount: dataset.queryCount, excludedQueryCount: dataset.excludedQueryCount,
      results, ...meanMetrics(results) };
  } finally { store.close(); }
}
function meanMetrics(results) {
  return Object.fromEntries(['recallAt100', 'ndcgAt10'].map(key => [key,
    results.reduce((sum, r) => sum + r[key], 0) / results.length]));
}
export function runWorker(config) {
  const { implementation, provenance } = loadPackage(config.package), manifest = config.manifest;
  const prepared = read(join(config.root, 'prepared.json'));
  assert.equal(prepared.workloadSha256, workloadHash(manifest)); assert.deepEqual(prepared.provenance, provenance);
  const before = snapshot(config.parentPid);
  const store = implementation.openNamespace(config.root, 'perf', SPEC, { readOnly: true });
  let cells;
  try {
    store.warmLexical();
    const shapes = manifest.queries.slice(0, 100), decoded = shapes.map(q => decodeRequest(q.request));
    for (let round = 0; round < SETTINGS.warmups; round++)
      for (const [i, q] of shapes.entries()) recordResult(store.query(decoded[i]), q.request);
    cells = shapes.map(q => ({ name: q.name, samplesMs: [], hits: undefined }));
    // Recorded shape order, once per round, with 1,000 samples for each shape.
    for (let round = 0; round < SETTINGS.samples; round++) {
      for (const [i, q] of shapes.entries()) {
        const start = process.hrtime.bigint(); const result = store.query(decoded[i]);
        const elapsed = process.hrtime.bigint() - start;
        const sample = Number(elapsed) / 1e6; positive(sample); cells[i].samplesMs.push(sample);
        const hits = recordResult(result, q.request);
        if (cells[i].hits) assert.deepEqual(hits, cells[i].hits, 'nondeterministic hits'); cells[i].hits = hits;
      }
    }
    cells = cells.map(c => ({ ...c, ...percentiles(c.samplesMs) }));
    for (const q of manifest.queries.slice(100)) {
      const cell = measureCell(store, q.request); cells.push({ name: q.name, ...cell, ...percentiles(cell.samplesMs) });
    }
  } finally { store.close(); }
  const beir = manifest.beir.map(d => measureBeir(implementation, config.root, d));
  const after = snapshot(config.parentPid);
  return { pid: process.pid, before, after, tainted: before.taints.length > 0 || after.taints.length > 0,
    cells, typeahead: percentiles(cells.slice(0, 100).flatMap(c => c.samplesMs)), beir };
}
export function runFive(config) {
  const worker = config.worker ?? ((_, i) => {
    const path = `${config.out}.run-${i + 1}.json`;
    assert.ok(!existsSync(path), 'repetition output already exists');
    const result = spawnSync(process.execPath, [fileURLToPath(import.meta.url), 'worker',
      '--package', config.package, '--root', config.root, '--manifest', config.manifestPath,
      '--out', path, '--parent-pid', String(process.pid)], { encoding: 'utf8', maxBuffer: 1024 * 1024 });
    writeFileSync(`${path}.log`, result.stderr ?? '', { flag: 'wx' });
    assert.equal(result.status, 0, `worker ${i + 1} failed: ${result.stderr}`); return read(path);
  });
  const runs = []; for (let i = 0; i < 5; i++) runs.push(worker(config, i));
  unique(runs.map(r => r.pid)); return runs;
}
export function validateMeasurement(v) {
  keys(v, 'schema manifest workloadSha256 provenance host settings root repetitions');
  assert.equal(v.schema, 'zeppelin-unified-perf-measurement-v1'); validateManifest(v.manifest);
  assert.equal(v.workloadSha256, workloadHash(v.manifest));
  assert.deepEqual(v.settings, SETTINGS); assert.ok(typeof v.root === 'string' && v.root.length);
  keys(v.provenance, 'source name version tag commit packageSha256 addonSha256 entrySha256 addonPath releaseFlags');
  assert.equal(v.provenance.source, 'npm'); assert.equal(v.provenance.name, '@zepdb/zeppelin-embed');
  assert.equal(v.provenance.version, '0.6.0'); assert.equal(v.provenance.tag, TAG); assert.equal(v.provenance.commit, COMMIT);
  for (const key of ['packageSha256', 'addonSha256', 'entrySha256']) assert.match(v.provenance[key], hashPattern);
  assert.ok(v.provenance.addonPath && v.provenance.releaseFlags);
  keys(v.host, 'cpu cores ramBytes os arch platform node rust');
  positive(v.host.cores); positive(v.host.ramBytes);
  for (const key of ['cpu', 'os', 'arch', 'platform', 'node', 'rust']) assert.ok(typeof v.host[key] === 'string' && v.host[key].length);
  assert.equal(v.repetitions.length, 5); unique(v.repetitions.map(r => r.pid));
  for (const r of v.repetitions) {
    keys(r, 'pid before after tainted cells typeahead beir'); assert.ok(Number.isInteger(r.pid) && r.pid > 0);
    assert.equal(r.tainted, false, 'tainted batch');
    for (const s of [r.before, r.after]) { keys(s, 'load ps taints'); assert.equal(s.load.length, 3);
      assert.ok(s.load.every(n => Number.isFinite(n) && n >= 0) && s.load[0] <= 3);
      assert.ok(typeof s.ps === 'string' && s.ps.length); assert.deepEqual(s.taints, []); }
    assert.deepEqual(r.cells.map(c => c.name), v.manifest.queries.map(q => q.name));
    for (const c of r.cells) {
      keys(c, 'name samplesMs p50 p95 p99 hits'); assert.equal(c.samplesMs.length, 1000); c.samplesMs.forEach(positive);
      for (const [key, val] of Object.entries(percentiles(c.samplesMs))) assert.equal(c[key], val);
      assert.ok(c.hits.length <= 10); unique(c.hits.map(h => h.id));
      for (const h of c.hits) { keys(h, 'id scoreBits'); assert.match(h.id, /^\d+$/); numberFromBits(h.scoreBits); }
    }
    assert.deepEqual(r.typeahead, percentiles(r.cells.slice(0, 100).flatMap(c => c.samplesMs)));
    assert.deepEqual(r.beir.map(d => d.name), DATASETS);
    for (const [i, d] of r.beir.entries()) {
      keys(d, 'name queryCount excludedQueryCount results recallAt100 ndcgAt10');
      const expected = v.manifest.beir[i]; assert.equal(d.queryCount, expected.queryCount);
      assert.equal(d.excludedQueryCount, expected.excludedQueryCount);
      assert.deepEqual(d.results.map(q => q.id), expected.queries.map(q => q.id));
      for (const [j, q] of d.results.entries()) {
        keys(q, 'id rankedIds scoreBits recallAt100 ndcgAt10'); assert.ok(q.rankedIds.length <= 100);
        assert.equal(q.scoreBits.length, q.rankedIds.length); q.scoreBits.forEach(numberFromBits);
        assert.ok(q.rankedIds.every(id => typeof id === 'string' && id.length));
        const metrics = beirMetrics(q.rankedIds, expected.queries[j].qrels);
        assert.equal(q.recallAt100, metrics.recallAt100); assert.equal(q.ndcgAt10, metrics.ndcgAt10);
      }
      const mean = meanMetrics(d.results); assert.equal(d.recallAt100, mean.recallAt100); assert.equal(d.ndcgAt10, mean.ndcgAt10);
    }
    assert.deepEqual(r.beir, v.repetitions[0].beir, 'BEIR differs between repetitions');
    assert.deepEqual(r.cells.map(c => c.hits), v.repetitions[0].cells.map(c => c.hits), 'hits differ between repetitions');
  }
}
async function main() {
  const [command, ...args] = process.argv.slice(2), config = {};
  const allowed = { prepare: ['package', 'root', 'beir', 'out'], measure: ['package', 'root', 'manifest', 'out'],
    validate: ['manifest', 'measurement'], worker: ['package', 'root', 'manifest', 'out', 'parent-pid'] }[command];
  assert.ok(allowed, 'use prepare, measure, or validate'); assert.equal(args.length, allowed.length * 2);
  for (let i = 0; i < args.length; i += 2) {
    const key = args[i].slice(2); assert.ok(args[i].startsWith('--') && allowed.includes(key) && !Object.hasOwn(config, key));
    config[key] = key === 'parent-pid' ? Number(args[i + 1]) : resolve(args[i + 1]);
  }
  if (command === 'prepare') await prepare(config);
  else {
    const manifestPath = config.manifest, manifest = read(manifestPath); await verifyArtifacts(manifest);
    if (command === 'validate') {
      const value = read(config.measurement); assert.deepEqual(value.manifest, manifest); validateMeasurement(value);
      console.log('valid: five complete fresh-process repetitions');
    } else if (command === 'worker') {
      freshJSON(config.out, runWorker({ ...config, manifest, parentPid: config['parent-pid'] }));
    } else {
      assert.ok(!existsSync(config.out), 'measurement output already exists');
      const prepared = read(join(config.root, 'prepared.json'));
      assert.equal(prepared.workloadSha256, workloadHash(manifest));
      const rust = spawnSync('rustc', ['--version'], { encoding: 'utf8' }); assert.equal(rust.status, 0);
      const host = { cpu: cpus()[0].model, cores: cpus().length, ramBytes: totalmem(), os: release(),
        platform: platform(), arch: arch(), node: process.version, rust: rust.stdout.trim() };
      const value = { schema: 'zeppelin-unified-perf-measurement-v1', manifest, workloadSha256: workloadHash(manifest),
        provenance: prepared.provenance, host, settings: SETTINGS, root: config.root,
        repetitions: runFive({ ...config, manifestPath }) };
      freshJSON(config.out, value); validateMeasurement(value); // preserve tainted output before refusing it
      console.log(`wrote ${config.out}`);
    }
  }
}
if (process.argv[1] && import.meta.url === pathToFileURL(resolve(process.argv[1])).href) {
  main().catch(error => { console.error(error); process.exitCode = 1; });
}
