// ZE-265: open and warm are timed separately; fixture construction is excluded.
import { createRequire } from 'node:module';
import { spawnSync, execFileSync } from 'node:child_process';
import { mkdtempSync, mkdirSync, readFileSync, readdirSync, statSync, writeFileSync } from 'node:fs';
import { createHash } from 'node:crypto';
import { tmpdir, cpus, totalmem, loadavg } from 'node:os';
import { join, resolve } from 'node:path';
import { fileURLToPath } from 'node:url';
import { performance } from 'node:perf_hooks';
import assert from 'node:assert/strict';
const { openNamespace } = createRequire(import.meta.url)('..');
const args = Object.fromEntries(process.argv.slice(2).reduce((pairs, arg, i, all) => {
  if (arg.startsWith('--')) pairs.push([arg.slice(2), all[i + 1]]);
  return pairs;
}, []));
function count(name, fallback) {
  const value = Number(args[name] ?? fallback);
  if (!Number.isSafeInteger(value) || value < 1) throw new Error(`${name} must be a positive integer`);
  return value;
}
const repetitions = count('repetitions', 5), reopens = count('reopens', 20), queries = count('queries', 100);
const output = resolve(args.output ?? '/tmp/ze265-synthetic');
const namespace = args.namespace ?? 'ze265';
const serialize = value => JSON.stringify(value, (_, item) => typeof item === 'bigint' ? item.toString() : item, 2);
function digest(root) {
  const hash = createHash('sha256');
  function visit(dir, relative = '') {
    for (const name of readdirSync(dir).sort()) {
      const path = join(dir, name), key = join(relative, name);
      if (statSync(path).isDirectory()) visit(path, key);
      else { hash.update(key); hash.update(readFileSync(path)); }
    }
  }
  visit(root);
  return hash.digest('hex');
}
function timed(run) { const start = performance.now(); const value = run(); return [performance.now() - start, value]; }
function percentile(samples, fraction) {
  const sorted = [...samples].sort((a, b) => a - b);
  return sorted[Math.ceil(sorted.length * fraction) - 1];
}
if (args.worker) {
  const root = args.store;
  const samples = [];
  for (const shape of ['exact', 'prefix']) {
    const request = { text: shape === 'exact' ? (args['exact-text'] ?? 'meeting harbour') : (args['prefix-text'] ?? 'meeting har'),
      lastAsPrefix: shape === 'prefix', k: 10 };
    let expected;
    for (let reopen = 0; reopen < reopens; reopen++) {
      for (const warmed of [false, true]) {
        const [openMs, store] = timed(() => openNamespace(root, namespace, { readOnly: true }));
        try {
          const warmStart = performance.now();
          if (warmed) await store.warmLexicalAsync();
          const warmMs = warmed ? performance.now() - warmStart : null;
          const [firstMs, first] = timed(() => store.query(request));
          const identity = serialize(first.hits.map(({ id, score }) => ({ id, score })));
          if (expected === undefined) expected = identity;
          assert.equal(identity, expected, 'fresh-handle IDs and scores');
          assert.ok(first.hits.length > 0, 'benchmark query must match');
          const repeatedMs = [];
          for (let query = 0; query < queries; query++) {
            const [ms, result] = timed(() => store.query(request));
            assert.equal(serialize(result.hits.map(({ id, score }) => ({ id, score }))), expected);
            repeatedMs.push(ms);
          }
          samples.push({ process: Number(args.worker), shape, warmed, reopen, openMs, warmMs,
            firstMs, repeatedMs, hits: first.hits.map(({ id, score }) => ({ id, score })),
            cacheBytes: null }); // Node exposes no stats API; core accounting is proved separately.
        } finally { store.close(); }
      }
    }
  }
  writeFileSync(join(output, `process-${args.worker}.json`), serialize(samples));
} else {
  mkdirSync(output, { recursive: true });
  const root = args.store ? resolve(args.store) : mkdtempSync(join(tmpdir(), 'ze265-fixture-'));
  let rows = null;
  if (!args.store) {
    rows = count('synthetic-rows', 150000);
    const store = openNamespace(root, namespace, {});
    try {
      for (let start = 0; start < rows; start += 1000) {
        const batch = Array.from({ length: Math.min(1000, rows - start) }, (_, i) => {
          const row = start + i;
          return { id: BigInt(row + 1), text: `note topic${row % 500} token${row} ${row % 500 === 0 ? 'meeting harbour' : 'ordinary document'}` };
        });
        store.upsert(batch);
        if ((start + batch.length) % 25000 === 0) store.seal();
      }
      store.seal(); store.merge();
    } finally { store.close(); }
  }
  const fixtureDigest = digest(root);
  const metadata = { root, namespace, rows, fixtureDigest, repetitions, reopens, queries,
    hardware: { platform: process.platform, arch: process.arch, cpu: cpus()[0]?.model,
      cores: cpus().length, memory: totalmem(), load: loadavg() },
    head: execFileSync('git', ['rev-parse', 'HEAD'], { encoding: 'utf8' }).trim(),
    osCache: 'Uncontrolled; fresh processes do not imply cold OS cache',
    cacheBytes: 'Unavailable through Node API; deterministic core accounting proof is separate',
    querySemantics: 'Existing Node multi-term scoring; synthetic markers always co-occur so exact matches equal the AND set' };
  writeFileSync(join(output, 'metadata.json'), serialize(metadata));
  const samples = [];
  for (let process = 1; process <= repetitions; process++) {
    const result = spawnSync(globalThis.process.execPath, [fileURLToPath(import.meta.url), '--worker', String(process),
      '--store', root, '--namespace', namespace, '--reopens', String(reopens), '--queries', String(queries), '--output', output,
      '--exact-text', args['exact-text'] ?? 'meeting harbour', '--prefix-text', args['prefix-text'] ?? 'meeting har'], { stdio: 'inherit' });
    if (result.status !== 0) throw new Error(`process ${process} failed`);
    samples.push(...JSON.parse(readFileSync(join(output, `process-${process}.json`), 'utf8')));
  }
  const summaries = [];
  for (const shape of ['exact', 'prefix']) for (const warmed of [false, true]) {
    const selected = samples.filter(s => s.shape === shape && s.warmed === warmed);
    const firstP95 = percentile(selected.map(s => s.firstMs), .95);
    const repeatedP95 = percentile(selected.flatMap(s => s.repeatedMs), .95);
    summaries.push({ shape, warmed, firstP95, repeatedP95, factor: firstP95 / repeatedP95,
      withinSmallFactor: firstP95 <= 3 * repeatedP95,
      openP95: percentile(selected.map(s => s.openMs), .95),
      warmP95: warmed ? percentile(selected.map(s => s.warmMs), .95) : null });
  }
  writeFileSync(join(output, 'summary.json'), serialize(summaries));
  console.log(serialize(summaries));
}
