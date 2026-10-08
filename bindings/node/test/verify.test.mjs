import assert from 'node:assert/strict';
import { spawnSync } from 'node:child_process';
import { createHash } from 'node:crypto';
import {
  mkdtempSync,
  readFileSync,
  readdirSync,
  rmSync,
  unlinkSync,
  writeFileSync,
} from 'node:fs';
import { createRequire } from 'node:module';
import { tmpdir } from 'node:os';
import { dirname, join } from 'node:path';
import test from 'node:test';
import { fileURLToPath } from 'node:url';

const require = createRequire(import.meta.url);
const { ZeppelinError, openNamespace, verify } = require('..');

const CLI = join(dirname(fileURLToPath(import.meta.url)), '..', 'bin', 'zeppelin-verify.js');
const SPEC = { attributes: [{ id: 1, name: 'meeting', type: 'u64' }] };
const OPTIONS = { durability: 'durable', commitTier: 'none' };
const WAL_HEADER_LEN = 40;
const FIRST_REGION = 16384;

/** A closed namespace with one sealed segment and an unsealed WAL tail. */
function fixture() {
  const root = mkdtempSync(join(tmpdir(), 'zeppelin-node-verify-'));
  const store = openNamespace(root, 'notes', SPEC, OPTIONS);
  const note = (id) => ({
    id: BigInt(id),
    text: `meeting ${id} about the harbour`,
    attributes: [{ id: 1, type: 'u64', value: BigInt(id) }],
  });
  store.upsert([1, 2, 3, 4, 5].map(note));
  store.seal();
  store.upsert([6, 7].map(note));
  store.delete([6n]);
  store.close();
  return { root, directory: join(root, 'notes') };
}

function hashes(directory) {
  return Object.fromEntries(
    readdirSync(directory)
      .sort()
      .map((name) => [
        name,
        createHash('sha256').update(readFileSync(join(directory, name))).digest('hex'),
      ]),
  );
}

function segment(directory) {
  const name = readdirSync(directory).find((file) => file.endsWith('.zseg'));
  assert.ok(name, 'fixture has a sealed segment');
  return name;
}

function flip(path, offset) {
  const bytes = readFileSync(path);
  bytes[offset] ^= 0x5a;
  writeFileSync(path, bytes);
}

function truncate(path, removed) {
  const bytes = readFileSync(path);
  writeFileSync(path, bytes.subarray(0, bytes.length - removed));
}

function cli(...args) {
  return spawnSync(process.execPath, [CLI, ...args], { encoding: 'utf8' });
}

test('verify reports a clean store and changes no byte of it', () => {
  const { root, directory } = fixture();
  try {
    const before = hashes(directory);
    const report = verify(directory);
    assert.deepEqual(report.findings, []);
    assert.equal(report.ok, true);
    assert.equal(report.segmentsChecked, 1n);
    // The seal truncates the WAL (ZE-233): only the tail records remain.
    assert.equal(report.walRecordsChecked, 3n);
    assert.equal(typeof report.generation, 'bigint');
    assert.ok(report.generation > 0n);
    assert.deepEqual(hashes(directory), before);

    const empty = join(root, 'empty');
    const store = openNamespace(root, 'empty', SPEC, OPTIONS);
    store.close();
    assert.equal(verify(empty).ok, true);
  } finally {
    rmSync(root, { recursive: true, force: true });
  }
});

test('verify names each kind of damage and still changes no byte', () => {
  const cases = [
    {
      name: 'manifest byte flip',
      damage: (directory) => flip(join(directory, 'manifest.ze'), 60),
      kind: 'manifestCorrupt',
      file: () => 'manifest.ze',
    },
    {
      name: 'manifest deleted',
      damage: (directory) => unlinkSync(join(directory, 'manifest.ze')),
      kind: 'manifestMissing',
      file: () => 'manifest.ze',
    },
    {
      // Regions start on 16 KiB boundaries, so the first begins at 16384.
      name: 'segment region byte flip',
      damage: (directory) => flip(join(directory, segment(directory)), FIRST_REGION + 4),
      kind: 'segmentRegionCorrupt',
      file: segment,
      offset: BigInt(FIRST_REGION),
    },
    {
      // Alignment padding between regions is covered by the file trailer.
      name: 'segment padding byte flip',
      damage: (directory) => flip(join(directory, segment(directory)), FIRST_REGION - 1),
      kind: 'segmentCorrupt',
      file: segment,
    },
    {
      name: 'segment truncated',
      damage: (directory) => truncate(join(directory, segment(directory)), 64),
      kind: 'segmentCorrupt',
      file: segment,
    },
    {
      name: 'segment deleted',
      damage: (directory) => unlinkSync(join(directory, segment(directory))),
      kind: 'segmentMissing',
      file: null,
    },
    {
      name: 'WAL record byte flip',
      damage: (directory) => flip(join(directory, 'wal.ze'), WAL_HEADER_LEN + 20),
      kind: 'walRecordCorrupt',
      file: () => 'wal.ze',
      offset: BigInt(WAL_HEADER_LEN),
    },
    {
      name: 'WAL torn tail',
      damage: (directory) => truncate(join(directory, 'wal.ze'), 3),
      kind: 'walRecordCorrupt',
      file: () => 'wal.ze',
    },
    {
      name: 'purge intent damaged',
      damage: (directory) => writeFileSync(join(directory, 'purge.ze'), 'not an intent'),
      kind: 'purgeIntentCorrupt',
      file: () => 'purge.ze',
    },
    {
      name: 'WAL deleted',
      damage: (directory) => unlinkSync(join(directory, 'wal.ze')),
      kind: 'walMissing',
      file: () => 'wal.ze',
    },
  ];
  for (const entry of cases) {
    const { root, directory } = fixture();
    try {
      const expectedFile = entry.file === null ? segment(directory) : null;
      entry.damage(directory);
      const before = hashes(directory);
      const report = verify(directory);
      assert.equal(report.ok, false, entry.name);
      assert.equal(report.findings.length, 1, `${entry.name}: ${JSON.stringify(report.findings, (_k, v) => (typeof v === 'bigint' ? `${v}` : v))}`);
      const [finding] = report.findings;
      assert.equal(finding.kind, entry.kind, entry.name);
      assert.equal(finding.file, expectedFile ?? entry.file(directory), entry.name);
      assert.equal(typeof finding.detail, 'string');
      assert.ok(finding.detail.length > 0, entry.name);
      if (entry.offset !== undefined) assert.equal(finding.offset, entry.offset, entry.name);
      assert.deepEqual(hashes(directory), before, `${entry.name}: verify changed the store`);
    } finally {
      rmSync(root, { recursive: true, force: true });
    }
  }
});

test('verify rejects input that does not name a store directory', () => {
  const root = mkdtempSync(join(tmpdir(), 'zeppelin-node-verify-args-'));
  try {
    assert.throws(() => verify(), { name: 'TypeError', code: 'ERR_INVALID_ARG_TYPE' });
    assert.throws(() => verify(42), { name: 'TypeError', code: 'ERR_INVALID_ARG_TYPE' });
    assert.throws(() => verify(''), (error) => {
      assert.ok(error instanceof ZeppelinError);
      assert.equal(error.code, 'ZE_ERR_INVALID_ARGUMENT');
      return true;
    });
    assert.throws(() => verify(`${root}\0x`), { code: 'ZE_ERR_INVALID_ARGUMENT' });
    assert.throws(() => verify(join(root, 'absent')), (error) => {
      assert.ok(error instanceof ZeppelinError);
      assert.equal(error.code, 'ZE_ERR_NOT_FOUND');
      assert.match(error.message, /does not exist/);
      return true;
    });
    assert.deepEqual(readdirSync(root), []);
    const file = join(root, 'file');
    writeFileSync(file, 'x');
    assert.throws(() => verify(file), { code: 'ZE_ERR_IO', message: /not a directory/ });
  } finally {
    rmSync(root, { recursive: true, force: true });
  }
});

test('zeppelin-verify prints the report and exits 0 clean, 1 damaged, 2 unusable', () => {
  const { root, directory } = fixture();
  try {
    const clean = cli(directory);
    assert.equal(clean.status, 0, clean.stderr);
    const report = JSON.parse(clean.stdout);
    assert.equal(report.ok, true);
    assert.equal(report.segmentsChecked, '1');
    assert.deepEqual(report.findings, []);

    flip(join(directory, 'wal.ze'), WAL_HEADER_LEN + 20);
    const damaged = cli(directory);
    assert.equal(damaged.status, 1, damaged.stderr);
    const [finding] = JSON.parse(damaged.stdout).findings;
    assert.equal(finding.kind, 'walRecordCorrupt');
    assert.equal(finding.offset, String(WAL_HEADER_LEN));

    const usage = cli();
    assert.equal(usage.status, 2);
    assert.match(usage.stderr, /usage: zeppelin-verify <store-directory>/);
    assert.equal(cli(directory, directory).status, 2);
    assert.equal(cli('--bogus').status, 2);
    const help = cli('--help');
    assert.equal(help.status, 0);
    assert.match(help.stdout, /usage/);
    const missing = cli(join(root, 'absent'));
    assert.equal(missing.status, 2);
    assert.match(missing.stderr, /ZE_ERR_NOT_FOUND/);
  } finally {
    rmSync(root, { recursive: true, force: true });
  }
});

test('verify reports graphObjectCorrupt without changing the directory', async (t) => {
  const { GraphStore } = require('..');
  if (!GraphStore.isSupported()) { t.skip('graph unavailable'); return; }
  const root = mkdtempSync(join(tmpdir(), 'zeppelin-node-verify-graph-'));
  const directory = join(root, 'store');
  try {
    const graph = GraphStore.open(directory, { autoReclaim: false });
    graph.apply([{ kind: 'node', operation: 'create', namespace: 'test', key: 'a', revision: 1n }]);
    graph.close();
    const file = readdirSync(directory).find(name => name.endsWith('.zgraph'));
    const path = join(directory, file);
    const bytes = readFileSync(path);
    bytes[100] ^= 1;
    writeFileSync(path, bytes);
    const before = hashes(directory);
    const report = verify(directory);
    assert.ok(report.findings.some(finding => finding.kind === 'graphObjectCorrupt' && finding.file === file), report.findings.map(finding => finding.kind).join(', '));
    assert.deepEqual(hashes(directory), before);
  } finally { rmSync(root, { recursive: true, force: true }); }
});
