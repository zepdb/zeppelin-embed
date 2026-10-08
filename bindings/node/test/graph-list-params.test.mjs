import { openGraph } from './graph-fixture.mjs';
import assert from 'node:assert/strict';
import { mkdtempSync, rmSync } from 'node:fs';
import { tmpdir } from 'node:os';
import { join } from 'node:path';
import { createRequire } from 'node:module';
import test from 'node:test';
const { Store, ZeppelinError } = createRequire(import.meta.url)('..');
test('Cypher IN accepts string list parameters', { skip: !Store.graphSupported() }, () => {
  const dir = mkdtempSync(join(tmpdir(), 'ze-list-'));
  const s = openGraph(join(dir, 'graph'));
  try {
    s.graphApply(['a', 'b'].map(id => ({ kind: 'node', operation: 'create', namespace: 'docs', key: id, revision: 1n, properties: { id } })));
    assert.deepEqual(s.cypher('MATCH (n) WHERE n.id IN $ids RETURN n.id', { ids: ['a'] }).rows, [['a']]);
  } finally { s.close(); rmSync(dir, { recursive: true, force: true }); }
});

test('list parameters preserve ABI values and bounds, synchronously and asynchronously', { skip: !Store.graphSupported() }, async () => {
  const dir = mkdtempSync(join(tmpdir(), 'ze-list-'));
  const s = openGraph(join(dir, 'graph'));
  const invalid = pattern => e => e instanceof ZeppelinError && e.code === 'ZE_ERR_INVALID_ARGUMENT' && pattern.test(e.message);
  try {
    s.graphApply([1n, 2n].map(id => ({ kind: 'node', operation: 'create', namespace: 'docs', key: String(id), revision: 1n, properties: { id } })));
    // The current engine expression arena has 1024 cells; stay within its budget.
    const query = 'MATCH (n) WHERE n.id IN $ids RETURN n.id';
    assert.deepEqual(s.cypher(query, { ids: [2n] }).rows, [[2n]]);
    assert.deepEqual(s.cypher(query, { ids: [1, 2] }).rows, [[1n], [2n]]);
    assert.deepEqual(s.cypher(query, { ids: [] }).rows, []);
    assert.deepEqual(s.cypher(query, { ids: new Float64Array() }).rows, []);
    assert.deepEqual(s.cypher(query, { ids: Array.from({ length: 1000 }, (_, i) => BigInt(i)) }).rows, [[1n], [2n]]);
    const nested = [[], ['a', null, true, -9007199254740993n, 1.25], [[2n]]];
    assert.deepEqual(s.cypher('RETURN $xs', { xs: nested }).rows, [[nested]]);
    for (const xs of [new Int8Array([1, 2]), new Uint32Array([1, 2]), new Float32Array([1, 2]), new Float64Array([1, 2]), new BigInt64Array([1n, 2n]), new BigUint64Array([1n, 2n])]) {
      assert.deepEqual(s.cypher(query, { ids: xs }).rows, [[1n], [2n]]);
    }
    for (const xs of [['ok', {}], [undefined], [Symbol('x')], [NaN], [1n << 63n], new BigUint64Array([1n << 63n])]) {
      assert.throws(() => s.cypher('RETURN $xs', { xs }), invalid(/unsupported parameter/));
    }
    let deep = 1n;
    for (let i = 0; i < 16; ++i) deep = [deep];
    assert.deepEqual(s.cypher('RETURN $xs', { xs: deep }).rows, [[deep]]);
    assert.throws(() => s.cypher('RETURN $xs', { xs: [deep] }), invalid(/depth exceeds 16/));
    const cycle = []; cycle.push(cycle);
    assert.throws(() => s.cypher('RETURN $xs', { xs: cycle }), invalid(/depth exceeds 16/));
    const huge = new Uint8Array(524289);
    assert.throws(() => s.cypher('RETURN $xs', { xs: huge }), invalid(/524288 elements/));
    assert.throws(() => s.cypher('RETURN $a, $b', { a: new Uint8Array(300000), b: new Uint8Array(300000) }), invalid(/524288 elements/));
    const controller = new AbortController();
    const ids = [1n, 2n];
    const pending = s.cypherAsync(query, { ids }, { signal: controller.signal });
    ids[0] = 9n;
    assert.deepEqual((await pending).rows, [[1n], [2n]]);
    controller.abort();
    await assert.rejects(s.cypherAsync(query, { ids: [1n] }, { signal: controller.signal }), e => e instanceof ZeppelinError && e.code === 'ZE_ERR_CANCELLED');
  } finally { s.close(); rmSync(dir, { recursive: true, force: true }); }
});
