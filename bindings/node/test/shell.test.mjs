import assert from 'node:assert/strict';
import { mkdtempSync, rmSync, readdirSync, readFileSync, statSync, cpSync, existsSync } from 'node:fs';
import { tmpdir } from 'node:os';
import { join } from 'node:path';
import { createRequire } from 'node:module';
import { spawnSync } from 'node:child_process';
import { fileURLToPath } from 'node:url';
import test from 'node:test';
const require = createRequire(import.meta.url);
const { Store, openInspection, openNamespace } = require('..');
const bin = fileURLToPath(new URL('../bin/zeppelin-shell.js', import.meta.url));
const attributes = ['u64', 'i64', 'f64', 'bool', 'dictionaryString', 'rawString'].map((type, i) => ({ id: i + 1, name: i === 0 ? 'rank' : `属性-${type}`, type, nullable: true }));
function fixture(body) {
  const root = mkdtempSync(join(tmpdir(), 'ze-shell-'));
  try {
    const store = openNamespace(root, 'docs', { attributes });
    store.upsert(Array.from({ length: 3 }, (_, i) => ({ id: BigInt(i + 1), text: 'hello world', attributes: [{ id: 1, type: 'u64', value: BigInt(i) }] })));
    store.seal();
    store.upsert([{ id: 3n, revision: 2n, text: 'hello again', attributes: [{ id: 1, type: 'u64', value: 2n }] }]);
    store.close();
    body(root);
  } finally { rmSync(root, { recursive: true, force: true }); }
}
function run(...args) { return spawnSync(process.execPath, [bin, ...args], { encoding: 'utf8' }); }
function json(...args) { const r = run(...args); assert.equal(r.status, 0, r.stderr); return JSON.parse(r.stdout); }
function snapshot(path) {
  return readdirSync(path).sort().flatMap(name => {
    const p = join(path, name), s = statSync(p);
    return s.isDirectory() ? snapshot(p).map(row => [name, ...row]) : [[name, s.mtimeMs, readFileSync(p).toString('hex')]];
  });
}
test('schema exposes persisted attributes on a read-only store', () => fixture(root => {
  const store = openInspection(join(root, 'docs'));
  try {
    assert.deepEqual(store.schema(), attributes);
    assert.throws(() => store.upsert([{ id: 4n }]), error => error.code === 'ZE_ERR_ACCESS_MODE');
    assert.throws(() => new Store(join(root, 'docs'), { readOnly: true }), error => error.code === 'ZE_ERR_EPOCH_UNDECLARED');
  } finally { store.close(); }
  assert.throws(() => store.schema(), error => error.code === 'ZE_ERR_CLOSED');
}));
test('shell inspects a copied store without changing files', () => fixture(root => {
  const copy = join(root, 'copy'); cpSync(join(root, 'docs'), join(copy, 'docs'), { recursive: true });
  const path = join(copy, 'docs'), before = snapshot(copy);
  assert.deepEqual(json(copy, 'namespaces'), [{ name: 'docs', attributes }]);
  assert.equal(json(path, 'get', '1').documents[0].id, '1');
  const request = JSON.stringify({ filter: { op: 'range', attributeId: 1, lower: { id: 1, type: 'u64', value: { $bigint: '1' } }, lowerInclusive: true }, order: { attributeId: 1, direction: 'descending' }, limit: 1 });
  assert.deepEqual(json(path, 'scan', request).documents.map(d => d.id), ['3']);
  assert.equal(json(path, 'count').count, '3');
  assert.ok(json(path, 'query', '{"text":"hello","k":2}').hits.length > 0);
  const dump = run(path, 'dump', '{"limit":1}'); assert.equal(dump.status, 0, dump.stderr);
  assert.deepEqual(dump.stdout.trim().split('\n').map(line => JSON.parse(line).id).sort(), ['1', '2', '3']);
  assert.equal(json(path, 'verify').ok, true);
  assert.deepEqual(snapshot(copy), before);
}));
test('shell rejects invalid requests and missing stores without creating files', () => fixture(root => {
  assert.equal(run(join(root, 'docs'), 'scan', '{').status, 2);
  assert.equal(run(join(root, 'docs'), 'erase').status, 2);
  const missing = join(root, 'missing');
  assert.equal(run(missing, 'count').status, 2); assert.equal(existsSync(missing), false);
}));
