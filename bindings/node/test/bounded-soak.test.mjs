import assert from 'node:assert/strict';
import { mkdtempSync, readdirSync, rmSync, statSync } from 'node:fs';
import { tmpdir } from 'node:os';
import { join } from 'node:path';
import { createRequire } from 'node:module';
import test from 'node:test';
const { openNamespace } = createRequire(import.meta.url)('..');

// Eight meeting hours: one transcript per second, one note edit per five
// seconds. Then simulate eight hours of note edits, holding live data fixed.
// No sleeps, forced GC, close/reopen, explicit seal or merge during measurement.
test('eight simulated meeting hours stay bounded automatically', { timeout: process.env.ZE_LONG_SOAK === '1' ? 1_800_000 : 180_000 }, async (t) => {
  const hours = process.env.ZE_LONG_SOAK === '1' ? 48 : 8;
  const root = mkdtempSync(join(tmpdir(), 'ze-bounded-soak-'));
  const store = openNamespace(root, 'notes', {}, {
    durability: 'durable', commitTier: 'none', autoSealRows: 256, autoMerge: true,
  });
  const dir = join(root, 'notes');
  const samples = [];
  const started = performance.now();
  const text = 'meeting transcript harbour decisions follow up actions '.repeat(3);
  try {
    for (let phase = 0; phase < 2; phase++) {
      for (let hour = 0; hour < hours; hour++) {
        for (let second = 0; second < 3600; second++) {
          const id = BigInt(hour * 3600 + second + 1);
          if (phase === 0) store.upsert([{ id, text }]);
          if (second % 60 === 0) {
            assert.ok(performance.now() - started < (hours === 8 ? 180_000 : 1_800_000), 'soak wall-clock budget');
            await new Promise(resolve => setImmediate(resolve));
          }
          if (second % 5 === 0) store.upsert([{ id: 0n,
            revision: BigInt((phase * hours + hour) * 720 + second / 5 + 1), text: `note ${text}` }]);
        }
        const files = readdirSync(dir);
        const sample = { phase, hour: hour + 1, rss: process.memoryUsage().rss,
          disk: files.reduce((sum, file) => sum + statSync(join(dir, file)).size, 0),
          wal: statSync(join(dir, 'wal.ze')).size,
          segments: files.filter(file => file.endsWith('.zseg')).length };
        samples.push(sample);
        assert.ok(sample.segments <= hours * 2, `segments ${sample.segments}`);
        assert.ok(sample.wal < 256 * 1024, `WAL ${sample.wal}`);
        assert.ok(sample.disk < hours * 4 * 1024 * 1024, `disk ${sample.disk}`);
      }
    }
    t.diagnostic(JSON.stringify({ hours, elapsedMs: performance.now() - started, samples }));
    const fixed = samples.filter(s => s.phase === 1);
    // Memory released by the allocator is not growth. Compare the high-water
    // mark with the start of the fixed-corpus phase, not its later minimum.
    const rssGrowth = Math.max(...fixed.map(s => s.rss)) - samples[hours - 1].rss;
    assert.ok(rssGrowth < 64 * 1024 * 1024, `fixed-corpus RSS growth ${rssGrowth}`);
    assert.ok(fixed.at(-1).disk < samples[hours - 1].disk * 2, 'revision disk must stay below twice live-corpus disk');
    assert.equal(store.count().count, BigInt(hours * 3600 + 1));
    assert.equal(store.get([0n]).documents[0].revision, BigInt(hours * 1440));
    assert.equal(store.query({ text: 'harbour', k: 5 }).hits.length, 5);
  } finally { store.close(); rmSync(root, { recursive: true, force: true }); }
});
