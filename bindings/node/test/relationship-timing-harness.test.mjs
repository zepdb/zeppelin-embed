import assert from 'node:assert/strict';
import { createHash } from 'node:crypto';
import { mkdtempSync, mkdirSync, readFileSync, rmSync, writeFileSync } from 'node:fs';
import { tmpdir } from 'node:os';
import { join, resolve } from 'node:path';
import { spawnSync } from 'node:child_process';
import { fileURLToPath } from 'node:url';
import test from 'node:test';

const harness = fileURLToPath(new URL('../../../scripts/cy_time.sh', import.meta.url));

function fixture(t, buildStatus = 0) {
  const root = mkdtempSync(join(tmpdir(), 'relationship-timing-'));
  t.after(() => rmSync(root, { recursive: true, force: true }));
  const worktree = join(root, 'worktree');
  const native = join(worktree, 'bindings/node');
  const platform = `${process.platform}-${process.arch}`;
  mkdirSync(join(native, 'prebuilds', platform), { recursive: true });
  mkdirSync(join(native, 'node_modules'));
  mkdirSync(join(native, 'bench'));
  mkdirSync(join(root, 'bin'));
  writeFileSync(join(native, 'prebuilds', platform, 'zeppelin_embed.node'), 'stale');
  const log = join(root, 'commands');
  for (const [name, body] of [
    ['npm', `echo "npm $*" >> "$HARNESS_LOG"\nif [ ${buildStatus} = 0 ]; then printf fresh > prebuilds/${platform}/zeppelin_embed.node; fi\nexit ${buildStatus}`],
    ['node', 'echo "node $*" >> "$HARNESS_LOG"'],
    ['git', 'echo 0123456789012345678901234567890123456789'],
  ]) {
    writeFileSync(join(root, 'bin', name), `#!/bin/sh\n${body}\n`, { mode: 0o755 });
  }
  const run = () => spawnSync('bash', [harness, worktree, resolve(root, 'stores')], {
    encoding: 'utf8',
    env: { ...process.env, PATH: `${join(root, 'bin')}:${process.env.PATH}`, HARNESS_LOG: log },
  });
  return { run, log };
}

test('timing_harness_rebuilds_an_existing_addon_on_every_run', t => {
  const { run, log } = fixture(t);
  assert.equal(run().status, 0);
  assert.equal(run().status, 0);
  const commands = readFileSync(log, 'utf8');
  assert.equal(commands.match(/npm run build:native/g)?.length, 2);
  assert.equal(commands.match(/node .*relationship-timing.mjs/g)?.length, 2);
});

test('timing_harness_refuses_to_time_a_failed_rebuild', t => {
  const { run, log } = fixture(t, 17);
  assert.notEqual(run().status, 0);
  const commands = readFileSync(log, 'utf8');
  assert.match(commands, /npm run build:native/);
  assert.doesNotMatch(commands, /node /);
});

test('timing_harness_records_head_and_addon_hash_before_timing', t => {
  const { run } = fixture(t);
  const result = run();
  assert.equal(result.status, 0);
  assert.match(result.stdout, /HEAD_SHA=0123456789012345678901234567890123456789/);
  const expectedHash = createHash('sha256').update('fresh').digest('hex');
  assert.ok(result.stdout.includes(`ADDON_SHA256=${expectedHash}`));
});

test('timing_table_accepts_bigint_query_values', t => {
  const root = mkdtempSync(join(tmpdir(), 'relationship-table-'));
  t.after(() => rmSync(root, { recursive: true, force: true }));
  mkdirSync(join(root, 'bench'));
  const source = new URL('../bench/relationship-timing.mjs', import.meta.url);
  writeFileSync(join(root, 'bench/relationship-timing.mjs'), readFileSync(source));
  writeFileSync(join(root, 'bench/client-parity-generator.mjs'), 'export const SPEC = {};');
  writeFileSync(join(root, 'index.js'), `exports.openNamespace = (_path, _name, _spec, options) => {
    if (!options.readOnly) throw new Error('timings require read-only access');
    return { cypher: () => ({ rows: [[1n]] }), close: () => {} };
  };`);
  const result = spawnSync(process.execPath, [join(root, 'bench/relationship-timing.mjs'), root], {
    encoding: 'utf8',
  });
  assert.equal(result.status, 0, result.stderr);
  const records = result.stdout.trim().split('\n').map(line => JSON.parse(line));
  assert.equal(records.length, 13);
  for (const record of records) {
    assert.deepEqual(record.firstRow, ['1']);
    assert.equal(record.milliseconds.length, 3);
    assert.ok(Number.isFinite(record.medianMs));
  }
});
