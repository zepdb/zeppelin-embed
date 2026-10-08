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
const CYPHER_WARMUPS = 1, CYPHER_SAMPLES = 10;
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
// These are declared candidate-only cells; the baseline manifest stays byte-compatible.
export function unifiedQueries(smoke = false) {
  const eligibleIds = Array.from({ length: smoke ? 100 : 10000 }, (_, i) =>
    String(Math.floor(i / 300) * 302 + i % 300 + 1));
  const cells = perfQueries().slice(100, 102).map(q => ({
    name: q.name.replace('unfiltered', 'eligible'), request: { ...q.request, eligibleIds } }));
  return [...cells, ...cells.map(q => ({ ...q, name: `cypher-${q.name}` }))];
}
function attachRelationships(store, count, stride) {
  store.enableGraph();
  const ids = [];
  for (let i = 0; i < count; i++) {
    const source = BigInt(stride * i + 1);
    const result = store.graphApply([{ kind: 'relationship', operation: 'create', namespace: 'perf',
      key: `perf-${i}`, revision: 1n, type: 'PERF_LINK', source, target: source + 1n }]);
    assert.equal(result.receipts.length, 1);
    assert.equal(result.receipts[0].kind, 'relationship'); assert.equal(result.receipts[0].deleted, false);
    ids.push(String(result.receipts[0].id));
  }
  unique(ids); return ids;
}
function verifyPopulation(store, rows, ids, stride) {
  assert.equal(ids.length, stride === 302 ? 500 : 1); unique(ids);
  assert.equal(store.count().count, BigInt(rows), 'relationship-only writes changed document count');
  const edges = store.graphGetRelationships(ids.map(BigInt));
  assert.equal(edges.length, ids.length);
  for (const [i, edge] of edges.entries()) {
    assert.ok(edge, 'missing relationship on graph-enabled Store');
    assert.equal(edge.source, BigInt(stride * i + 1)); assert.equal(edge.target, edge.source + 1n);
    assert.equal(edge.key, `perf-${i}`); assert.equal(edge.type, 'PERF_LINK');
  }
}
export function prepareUnifiedPerfStore(implementation, directory, manifest = {}, onBatch = () => {}, batches = documentBatches()) {
  preparePerfStore(implementation, directory, onBatch, batches);
  let store = implementation.openNamespace(directory, 'perf', SPEC, { autoMerge: false });
  let ids;
  try { ids = attachRelationships(store, 500, 302); verifyPopulation(store, 150000, ids, 302); }
  finally { store.close(); }
  store = implementation.openNamespace(directory, 'perf', SPEC, { readOnly: true });
  try { verifyPopulation(store, 150000, ids, 302); } finally { store.close(); }
  // IDs are receipts, never guessed allocator values. They are checked again by every worker.
  manifest.perfRelationships = ids;
}
function cypherCall(request) {
  const hybrid = Boolean(request.vector);
  const call = hybrid ? "ze.hybrid_search($vector, $text, 10, 'exact', eligible)" : 'ze.text_search($text, 10, eligible)';
  return { text: `MATCH (d:Document) WHERE ze.node_id(d) <= $lastId WITH collect(DISTINCT d) AS eligible CALL ${call} YIELD node, score RETURN ze.node_id(node) AS id, score`,
    parameters: { lastId: request.eligibleIds.at(-1).toString(16).padStart(32, '0'), text: request.text, ...(hybrid ? { vector: request.vector } : {}) } };
}
function recordCypherResult(result) {
  assert.deepEqual(result.columns, ['id', 'score']); assert.equal(result.receipts.length, 0);
  assert.ok(result.rows.length <= 10);
  const hits = result.rows.map(row => {
    assert.equal(row.length, 2); const [id, score] = row; assert.match(id, /^[a-f0-9]{32}$/);
    const numeric = BigInt(`0x${id}`); assert.ok(numeric > 0n);
    return { id: String(numeric), scoreBits: bits(score) };
  });
  unique(hits.map(h => h.id)); return hits;
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
export function measureCell(store, request, warmups = 20, samples = 1000, cypher = false) {
  assert.ok(Number.isInteger(warmups) && warmups >= 0 && Number.isInteger(samples) && samples > 0);
  const decoded = decodeRequest(request); // decoding/allocation is outside the timer
  if (cypher) assert.deepEqual(request.eligibleIds, unifiedQueries(request.eligibleIds.length === 100)[0].request.eligibleIds, 'Cypher eligible prefix');
  const call = cypher ? cypherCall(decoded) : undefined;
  const run = () => cypher ? store.cypher(call.text, call.parameters) : store.query(decoded);
  const record = result => cypher ? recordCypherResult(result) : recordResult(result, request);
  for (let i = 0; i < warmups; i++) record(run());
  const samplesMs = []; let hits;
  for (let i = 0; i < samples; i++) {
    const start = process.hrtime.bigint();
    const result = run();
    const elapsed = process.hrtime.bigint() - start;
    const sample = Number(elapsed) / 1e6; positive(sample); samplesMs.push(sample);
    const observed = record(result);
    if (hits) assert.deepEqual(observed, hits, 'nondeterministic hits');
    hits = observed;
  }
  return { samplesMs, hits };
}

function loadPackage(path) {
  const checkout = resolve(path) === resolve(dirname(fileURLToPath(import.meta.url)), '..');
  path = resolve(path); const require = createRequire(import.meta.url);
  const metadata = read(join(path, 'package.json'));
  assert.equal(metadata.name, '@zepdb/zeppelin-embed');
  if (!checkout) { assert.equal(metadata.version, '0.6.0'); if (metadata.gitHead) assert.equal(metadata.gitHead, COMMIT); }
  const revision = checkout ? spawnSync('git', ['rev-parse', 'HEAD'], { cwd: path, encoding: 'utf8' }) : null;
  if (checkout) assert.equal(revision.status, 0);
  const implementation = require(path);
  const addons = Object.keys(require.cache).filter(p => p.startsWith(`${path}/`) && p.endsWith('.node'));
  assert.equal(addons.length, 1, 'identify the addon actually loaded');
  return { implementation, provenance: { source: checkout ? 'checkout' : 'npm', name: metadata.name, version: metadata.version,
    tag: checkout ? null : TAG, commit: checkout ? revision.stdout.trim() : COMMIT, packageSha256: sha(readFileSync(join(path, 'package.json'))),
    entrySha256: sha(readFileSync(require.resolve(path))), addonSha256: sha(readFileSync(addons[0])),
    addonPath: addons[0], releaseFlags: checkout ? 'npm run build:native (release)' : 'published npm prebuild; flags unavailable' } };
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
async function prepareBeir(implementation, root, directory, name, output, unified) {
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
    if (unified) {
      assert.ok(corpus.length >= 2); unified.beir[name] = attachRelationships(store, 1, 1);
      verifyPopulation(store, corpus.length, unified.beir[name], 1);
    }
  } finally { store.close(); }
  if (unified) {
    const reopened = implementation.openNamespace(root, name, spec, { readOnly: true });
    try { verifyPopulation(reopened, corpus.length, unified.beir[name], 1); } finally { reopened.close(); }
  }
  const mappingPath = join(output, `${name}-mapping.json`), queryPath = join(output, `${name}-queries.json`);
  freshJSON(mappingPath, mapping); freshJSON(queryPath, queries);
  return { name, spec, corpusCount: corpus.length, queryCount: queries.length, excludedQueryCount,
    inputs, mapping: await artifact(mappingPath), queryFile: await artifact(queryPath), queries };
}
async function prepare(config, isUnified = false) {
  assert.ok(!existsSync(config.root), 'root already exists; use a fresh root');
  assert.ok(!existsSync(config.out), 'manifest already exists');
  const { implementation, provenance } = loadPackage(config.package);
  assert.equal(provenance.source, isUnified ? 'checkout' : 'npm');
  const unified = isUnified ? { beir: {} } : undefined;
  mkdirSync(dirname(config.out), { recursive: true }); mkdirSync(config.root);
  const corpusPath = join(dirname(config.out), 'perf-rows.jsonl');
  const fd = openSync(corpusPath, 'wx');
  const prepareStore = isUnified ? (impl, root, onBatch) => prepareUnifiedPerfStore(impl, root, unified, onBatch) : preparePerfStore;
  try { prepareStore(implementation, config.root, batch => {
    for (const row of batch) writeSync(fd, json({ ...row, id: String(row.id), revision: String(row.revision),
      timestamp: String(row.timestamp), vector: Array.from(row.vector),
      attributes: row.attributes.map(a => ({ ...a, value: String(a.value) })) }));
  }); } finally { closeSync(fd); }
  const queries = perfQueries(), queryPath = join(dirname(config.out), 'perf-queries.json');
  freshJSON(queryPath, queries);
  const beir = [];
  for (const name of DATASETS) beir.push(await prepareBeir(implementation, config.root, config.beir, name, dirname(config.out), unified));
  const manifest = { schema: 'zeppelin-unified-perf-v1', seed, dimensions, attributes: SPEC.attributes,
    epoch: EPOCH, rowCount: 150000, segmentBoundaries: BOUNDARIES,
    corpus: await artifact(corpusPath), queryFile: await artifact(queryPath), queries, beir };
  freshJSON(config.out, manifest);
  freshJSON(join(config.root, 'prepared.json'), { workloadSha256: workloadHash(manifest), provenance, ...(unified ? { unified } : {}) });
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
function measureBeir(implementation, root, dataset, relationships) {
  const mapping = read(dataset.mapping.path); assert.equal(mapping.length, dataset.corpusCount); unique(mapping);
  const store = implementation.openNamespace(root, dataset.name, dataset.spec, { readOnly: true });
  try {
    if (relationships) verifyPopulation(store, dataset.corpusCount, relationships, 1);
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
    if (prepared.unified) verifyPopulation(store, 150000, prepared.unified.perfRelationships, 302);
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
    for (const q of [...manifest.queries.slice(100), ...(prepared.unified ? unifiedQueries() : [])]) {
      const isCypher = q.name.startsWith('cypher-'); // one Cypher query is a 150k-document scan (about 6 s): few samples
      const cell = measureCell(store, q.request, isCypher ? CYPHER_WARMUPS : SETTINGS.warmups, isCypher ? CYPHER_SAMPLES : SETTINGS.samples, isCypher);
      if (q.name.startsWith('cypher-')) assert.deepEqual(cell.hits, cells.find(c => c.name === q.name.slice(7)).hits, 'Cypher/Store eligible parity');
      cells.push({ name: q.name, ...cell, ...percentiles(cell.samplesMs) });
    }
  } finally { store.close(); }
  const beir = manifest.beir.map(d => measureBeir(implementation, config.root, d, prepared.unified?.beir[d.name]));
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
  keys(v, `schema manifest workloadSha256 provenance host settings root repetitions${v.provenance?.source === 'checkout' ? ' candidateOnlyQueries' : ''}`);
  assert.equal(v.schema, 'zeppelin-unified-perf-measurement-v1'); validateManifest(v.manifest);
  assert.equal(v.workloadSha256, workloadHash(v.manifest));
  assert.deepEqual(v.settings, SETTINGS); assert.ok(typeof v.root === 'string' && v.root.length);
  keys(v.provenance, 'source name version tag commit packageSha256 addonSha256 entrySha256 addonPath releaseFlags');
  assert.equal(v.provenance.name, '@zepdb/zeppelin-embed');
  assert.ok(['npm', 'checkout'].includes(v.provenance.source));
  if (v.provenance.source === 'npm') {
    assert.equal(v.provenance.version, '0.6.0'); assert.equal(v.provenance.tag, TAG); assert.equal(v.provenance.commit, COMMIT);
  } else {
    assert.deepEqual(v.candidateOnlyQueries, unifiedQueries(), 'candidate-only query schema');
    assert.equal(v.provenance.tag, null); assert.match(v.provenance.commit, /^[a-f0-9]{40}$/);
    assert.match(v.provenance.version, /^\d+\.\d+\.\d+$/); assert.equal(v.provenance.releaseFlags, 'npm run build:native (release)');
  }
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
    assert.deepEqual(r.cells.map(c => c.name), [...v.manifest.queries, ...(v.provenance.source === 'checkout' ? unifiedQueries() : [])].map(q => q.name));
    for (const c of r.cells) {
      keys(c, 'name samplesMs p50 p95 p99 hits'); assert.equal(c.samplesMs.length, c.name.startsWith('cypher-') ? CYPHER_SAMPLES : 1000); c.samplesMs.forEach(positive);
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
const WORK = { lexical: ['docs_evaluated', 'postings_decoded', 'blocks_decoded'],
  scan: ['dims_touched', 'bytes_read'], graph: ['candidates_scored', 'candidates_rescored'],
  hybrid: ['lexical_full_materializations', 'vector_candidates_produced', 'lexical_candidates_produced',
    'total_cross_filled_vector', 'total_cross_filled_lexical'], fusion: ['rounds'] };
function validateCounters(receipt, measurement, candidate) {
  const extra = 'acceptance unified rowCount graph eligibilityAccounting candidateOnlyQueries';
  keys(receipt, `schema workloadSha256 manifestSha256 revision binarySha256 optLevel warmups samples threadBudget queries${candidate ? ` ${extra}` : ''}`);
  assert.equal(receipt.schema, 'zeppelin-unified-perf-counters-v1');
  assert.equal(receipt.workloadSha256, measurement.workloadSha256);
  assert.equal(receipt.revision, measurement.provenance.commit, 'counter binary revision');
  for (const key of ['manifestSha256', 'binarySha256']) assert.match(receipt[key], hashPattern);
  assert.equal(receipt.optLevel, '3', 'release counters required');
  assert.equal(receipt.warmups, 20); assert.equal(receipt.samples, 5); assert.equal(receipt.threadBudget, 1);
  if (candidate) {
    assert.deepEqual(receipt.candidateOnlyQueries, unifiedQueries().slice(0, 2), 'counter candidate-only query schema');
    assert.equal(receipt.acceptance, true, 'smoke is not acceptance'); assert.equal(receipt.unified, true);
    assert.equal(receipt.rowCount, 150000); assert.deepEqual(receipt.graph, { documents: 150000, relationships: 500 });
    assert.equal(receipt.eligibilityAccounting, 'query latency includes construction; work counters omit eligibility-map construction');
  }
  const queries = [...perfQueries(), ...(candidate ? unifiedQueries().slice(0, 2) : [])];
  assert.deepEqual(receipt.queries.map(q => q.name), queries.map(q => q.name));
  for (const [i, q] of receipt.queries.entries()) {
    keys(q, 'name samples descriptive hits'); assert.equal(q.samples.length, 5); assert.equal(q.descriptive.length, 5);
    assert.deepEqual(q.hits, measurement.repetitions[0].cells.find(c => c.name === q.name).hits, 'Node/counter hit parity');
    for (const sample of q.samples) {
      keys(sample, Object.keys(WORK).join(' '));
      for (const [group, fields] of Object.entries(WORK)) {
        if (['hybrid', 'fusion'].includes(group) && !queries[i].request.vector) { assert.equal(sample[group], null); continue; }
        keys(sample[group], fields.join(' '));
        for (const field of fields) assert.ok(Number.isSafeInteger(sample[group][field]) && sample[group][field] >= 0, 'invalid work counter');
      }
      assert.deepEqual(sample, q.samples[0], 'nondeterministic counters');
    }
  }
}
function boundedWork(before, after, numerator, denominator, name) {
  for (const group of Object.keys(WORK)) {
    if (before[group] === null) { assert.equal(after[group], null); continue; }
    for (const field of WORK[group]) assert.ok(BigInt(after[group][field]) * BigInt(denominator) <=
      BigInt(before[group][field]) * BigInt(numerator), `${name}: ${group}.${field} work regression`);
  }
}
export function comparePerf(reference, candidate, referenceCounters, candidateCounters) {
  validateMeasurement(reference); validateMeasurement(candidate);
  assert.equal(reference.provenance.source, 'npm'); assert.equal(candidate.provenance.source, 'checkout');
  assert.notEqual(reference.provenance.commit, candidate.provenance.commit);
  assert.notEqual(reference.provenance.addonSha256, candidate.provenance.addonSha256, 'candidate addon must differ from release');
  assert.notEqual(referenceCounters.binarySha256, candidateCounters.binarySha256, 'candidate counters binary must differ from release');
  assert.equal(candidate.workloadSha256, reference.workloadSha256, 'logical workload mismatch');
  for (const key of ['cpu', 'cores', 'ramBytes', 'os', 'arch', 'platform', 'node'])
    assert.equal(candidate.host[key], reference.host[key], `host ${key} mismatch`);
  assert.deepEqual(candidate.settings, reference.settings);
  validateCounters(referenceCounters, reference, false); validateCounters(candidateCounters, candidate, true);
  // All correctness/provenance checks precede ratios. Metrics compare IEEE bits, including signed zero.
  for (const r of candidate.repetitions) {
    assert.deepEqual(r.beir, reference.repetitions[0].beir, 'BEIR ID/score/metric drift');
    assert.deepEqual(r.cells.slice(0, 104).map(c => c.hits), reference.repetitions[0].cells.map(c => c.hits), 'common result drift');
    for (const [i, d] of r.beir.entries()) {
      const b = reference.repetitions[0].beir[i];
      for (const metric of ['recallAt100', 'ndcgAt10']) {
        assert.equal(bits(d[metric]), bits(b[metric]));
        for (const [j, q] of d.results.entries()) assert.equal(bits(q[metric]), bits(b.results[j][metric]));
      }
    }
    for (const q of unifiedQueries().slice(2)) assert.deepEqual(r.cells.find(c => c.name === q.name).hits,
      r.cells.find(c => c.name === q.name.slice(7)).hits, 'Cypher eligible result drift');
  }
  for (const [i, q] of referenceCounters.queries.entries())
    boundedWork(q.samples[0], candidateCounters.queries[i].samples[0], 11, 10, q.name);
  for (const q of candidateCounters.queries.slice(104)) {
    const unfiltered = candidateCounters.queries.find(c => c.name === q.name.replace('eligible', 'unfiltered'));
    boundedWork(unfiltered.samples[0], q.samples[0], 2, 1, q.name);
  }
  for (const r of candidate.repetitions) {
    assert.ok(r.typeahead.p95 <= 30, 'type-ahead aggregate p95 > 30 ms');
    for (const c of r.cells.slice(0, 100)) assert.ok(c.p95 <= 30, `${c.name} p95 > 30 ms`);
  }
  const median = (m, name, key) => percentile(m.repetitions.map(r => r.cells.find(c => c.name === name)[key]), .5);
  const ratios = {};
  for (const q of perfQueries().slice(100)) {
    ratios[q.name] = {};
    for (const key of ['p50', 'p95']) {
      const b = median(reference, q.name, key), c = median(candidate, q.name, key);
      assert.ok(c <= b * 1.1, `${q.name} median ${key} > 1.1x`); ratios[q.name][key] = c / b;
    }
  }
  for (const q of unifiedQueries()) {
    const unfiltered = q.name.replace('cypher-', '').replace('eligible', 'unfiltered');
    assert.ok(median(candidate, q.name, 'p95') <= 2 * median(candidate, unfiltered, 'p95'), `${q.name} median p95 > 2x`);
  }
  return { ratios, typeaheadHeadroom2x: candidate.repetitions.every(r => r.typeahead.p95 <= 15 &&
    r.cells.slice(0, 100).every(c => c.p95 <= 15)), eligibilityAccounting: candidateCounters.eligibilityAccounting };
}
async function main() {
  const [command, ...args] = process.argv.slice(2), config = {};
  const allowed = { compare: ['baseline', 'candidate', 'baseline-counters', 'candidate-counters'],
    'prepare-unified': ['package', 'root', 'beir', 'out'], prepare: ['package', 'root', 'beir', 'out'], measure: ['package', 'root', 'manifest', 'out'],
    validate: ['manifest', 'measurement'], worker: ['package', 'root', 'manifest', 'out', 'parent-pid'] }[command];
  assert.ok(allowed, 'use prepare, prepare-unified, measure, validate, or compare'); assert.equal(args.length, allowed.length * 2);
  for (let i = 0; i < args.length; i += 2) {
    const key = args[i].slice(2); assert.ok(args[i].startsWith('--') && allowed.includes(key) && !Object.hasOwn(config, key));
    config[key] = key === 'parent-pid' ? Number(args[i + 1]) : resolve(args[i + 1]);
  }
  if (command === 'compare') {
    console.log(JSON.stringify(comparePerf(read(config.baseline), read(config.candidate), read(config['baseline-counters']), read(config['candidate-counters'])), null, 2));
  } else if (command === 'prepare' || command === 'prepare-unified') await prepare(config, command === 'prepare-unified');
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
        ...(prepared.unified ? { candidateOnlyQueries: unifiedQueries() } : {}),
        repetitions: runFive({ ...config, manifestPath }) };
      freshJSON(config.out, value); validateMeasurement(value); // preserve tainted output before refusing it
      console.log(`wrote ${config.out}`);
    }
  }
}
if (process.argv[1] && import.meta.url === pathToFileURL(resolve(process.argv[1])).href) {
  main().catch(error => { console.error(error); process.exitCode = 1; });
}
