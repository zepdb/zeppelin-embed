// Run: node bench/query-filter.mjs [notes=500] [segmentsPerNote=300] [iterations=30]
// Wall time is supporting evidence, not a regression threshold.
import assert from 'node:assert/strict';
import { mkdtempSync, rmSync } from 'node:fs';
import { tmpdir, cpus } from 'node:os';
import { join } from 'node:path';
import { createRequire } from 'node:module';
import { performance } from 'node:perf_hooks';
const { openNamespace } = createRequire(import.meta.url)('..');
const [notes, segments, iterations] = process.argv.slice(2).map(Number);
const n = notes ?? 500, perNote = segments ?? 300, runs = iterations ?? 30;
assert.ok([n, perNote, runs].every(v => Number.isSafeInteger(v) && v > 0));
const root = mkdtempSync(join(tmpdir(), 'zeppelin-query-filter-bench-'));
const store = openNamespace(root, 'notes', {
  vectorSpace: { dimensions: 2 },
  attributes: [{ id: 1, name: 'note', type: 'u64', nullable: false }],
});
try {
  for (let note = 0; note < n; note++) {
    store.upsert(Array.from({ length: perNote }, (_, segment) => ({
      id: BigInt(note * perNote + segment + 1), timestamp: BigInt(segment),
      text: `meeting harbour project update segment ${segment}`,
      vector: new Float32Array([note / n, segment / perNote]),
      attributes: [{ id: 1, type: 'u64', value: BigInt(note) }],
    })));
  }
  store.seal();
  const filter = { op: 'eq', attributeId: 1, values: [{ id: 1, type: 'u64', value: 0n }] };
  const timestampRange = { start: 0n, end: BigInt(Math.ceil(perNote / 2)) };
  console.log(JSON.stringify({ cpu: cpus()[0]?.model, platform: process.platform,
    arch: process.arch, node: process.version, notes: n, segmentsPerNote: perNote, iterations: runs }));
  for (const mode of ['lexical', 'hybrid']) {
    const base = { text: 'harbour', k: 10,
      ...(mode === 'hybrid' ? { vector: new Float32Array([0, 0]) } : {}) };
    for (const filtered of [false, true]) {
      const request = { ...base, ...(filtered ? { filter, timestampRange } : {}) };
      const query = () => {
        const result = store.query(request);
        assert.equal(result.hits.length, Math.min(10, filtered ? Number(timestampRange.end) : n * perNote));
        if (filtered) for (const hit of result.hits) assert.ok(hit.id >= 1n && hit.id <= timestampRange.end);
      };
      for (let i = 0; i < 5; i++) query();
      const samplesMs = [];
      for (let i = 0; i < runs; i++) {
        const start = performance.now(); query(); samplesMs.push(performance.now() - start);
      }
      const sorted = [...samplesMs].sort((a, b) => a - b);
      console.log(JSON.stringify({ mode, filtered, medianMs: sorted[Math.floor(runs / 2)], samplesMs }));
    }
  }
} finally { store.close(); rmSync(root, { recursive: true, force: true }); }
