import { openGraph } from './graph-fixture.mjs';
import assert from 'node:assert/strict';
import { mkdtempSync, readdirSync, rmSync, statSync } from 'node:fs';
import { tmpdir } from 'node:os';
import { join } from 'node:path';
import test from 'node:test';
import { Store } from '../index.js';

function roundTrip(count) {
  const batchSize = 100;
  const directory = mkdtempSync(join(tmpdir(), 'ze-257-'));
  const path = join(directory, 'graph');
  let store;
  let buildMs = 0;
  function reopenAndCheck(built) {
    const closeBegin = performance.now();
    store.close();
    const closeMs = performance.now() - closeBegin;
    const files = readdirSync(path, { withFileTypes: true });
    let storeBytes = 0;
    for (const file of files) {
      assert.ok(file.isFile(), `unexpected store subdirectory: ${file.name}`);
      storeBytes += statSync(join(path, file.name)).size;
    }
    const begin = performance.now();
    store = openGraph(path, {});
    const reopenMs = performance.now() - begin;
    assert.deepEqual(store.cypher('MATCH (n:Segment) RETURN count(n)').rows, [[BigInt(built)]]);
    assert.deepEqual(store.cypher('MATCH ()-[r:NEXT]->() RETURN count(r)').rows,
      [[BigInt(Math.ceil(built / batchSize))]]);
    assert.deepEqual(store.cypher('MATCH (n:Segment) RETURN n.startMs AS t ORDER BY t DESC LIMIT 10').rows,
      Array.from({ length: 10 }, (_, i) => [BigInt(built - 1 - i)]));
    console.log(JSON.stringify({ nodes: built, buildMs, closeMs, reopenMs, storeBytes, storeFiles: files.length, maxRssKiB: process.resourceUsage().maxRSS }));
  }
  try {
    store = openGraph(path);
    for (let start = 0; start < count; start += batchSize) {
      const batchBegin = performance.now();
      const size = Math.min(batchSize, count - start);
      if (start % 10000 === 0) console.log(`building ${count}: ${start}`);
      const nodes = Array.from({ length: size }, (_, i) => ({
        kind: 'node', operation: 'create', namespace: 'docs', key: String(start + i),
        revision: 1n, labels: ['Segment'], properties: { startMs: BigInt(start + i) },
      }));
      const edge = {
        kind: 'relationship', operation: 'create', namespace: 'edges', key: String(start),
        revision: 1n, type: 'NEXT', source: { local: 0 }, target: { local: 1 },
      };
      try { assert.equal(store.graphApply([...nodes, edge]).disposition, 'Committed'); }
      catch (error) { error.message += ` at batch ${start}`; throw error; }
      buildMs += performance.now() - batchBegin;
    }
    reopenAndCheck(count);
  } finally {
    store?.close();
    if (process.env.ZE_GRAPH_SCALE_KEEP) console.log(`fixture: ${path}`);
    else rmSync(directory, { recursive: true, force: true });
  }
}

test('graph scale: 20000 documents and edges reopen under default budgets',
  { skip: !Store.graphSupported() }, () => roundTrip(20_000));
test('graph scale: configurable large store reopens under default budgets',
  { skip: !Store.graphSupported() || process.env.ZE_GRAPH_SCALE !== '1' },
  () => {
    const count = Number(process.env.ZE_GRAPH_SCALE_COUNT ?? '150000');
    assert.ok(Number.isSafeInteger(count) && count >= 100 && count % 100 === 0,
      'ZE_GRAPH_SCALE_COUNT must be a positive multiple of 100');
    roundTrip(count);
  });
