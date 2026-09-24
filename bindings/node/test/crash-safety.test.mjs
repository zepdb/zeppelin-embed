// Crash-safety suite: a child process runs a seeded workload of upserts,
// deletes and seals against one namespace, and this parent kills it at a
// seeded operation. On macOS `child.kill('SIGKILL')` is kill -9; on Windows
// Node implements every `kill()` as TerminateProcess, so the same call is an
// unconditional termination there too. The parent then verifies the store,
// reopens it, and compares every document with an oracle built from the
// acknowledged operations: each acknowledged write is present, and the one
// operation in flight at the kill is either wholly present or wholly absent.
//
// ZE_TEST_SEED replays a run; ZE_CRASH_ROUNDS sets the number of kills;
// ZE_CRASH_EVIDENCE names a JSON file that receives the per-round results.

import assert from 'node:assert/strict';
import { spawn } from 'node:child_process';
import { mkdtempSync, rmSync, writeFileSync } from 'node:fs';
import { createRequire } from 'node:module';
import { tmpdir } from 'node:os';
import { performance } from 'node:perf_hooks';
import { dirname, join } from 'node:path';
import test from 'node:test';
import { fileURLToPath } from 'node:url';

const require = createRequire(import.meta.url);
const { openNamespace, verify } = require('..');
const { NAMESPACE, OPTIONS, SPEC, apply, rng, workload } = require('./crash-workload.cjs');

const CHILD = join(dirname(fileURLToPath(import.meta.url)), 'crash-child.cjs');
const OPERATIONS = 80;
const TYPES = ['upsert', 'delete', 'seal'];

function parseSeed(text) {
  if (text === undefined || text === '') return 0x2260_2026;
  const seed = Number(text);
  if (!Number.isSafeInteger(seed) || seed < 0 || seed > 0xffff_ffff) {
    throw new RangeError(`ZE_TEST_SEED must be an integer in 0..=4294967295, got ${text}`);
  }
  return seed;
}

function parseRounds(text) {
  if (text === undefined || text === '') return 60;
  const rounds = Number(text);
  if (!Number.isSafeInteger(rounds) || rounds < 1) {
    throw new RangeError(`ZE_CRASH_ROUNDS must be a positive integer, got ${text}`);
  }
  return rounds;
}

/**
 * Spawns the child and kills it `delayMicros` after it announces operation
 * `target`. The parent spins rather than sleeping so sub-millisecond delays
 * are honoured; the child keeps running on its own while the parent spins.
 */
function runUntilKilled(root, seed, target, delayMicros) {
  return new Promise((resolve, reject) => {
    const child = spawn(process.execPath, [CHILD, root, String(seed), String(OPERATIONS)], {
      stdio: ['ignore', 'pipe', 'pipe'],
    });
    let stdout = '';
    let stderr = '';
    let killed = false;
    child.stdout.setEncoding('utf8');
    child.stderr.setEncoding('utf8');
    child.stdout.on('data', (chunk) => {
      stdout += chunk;
      if (!killed && stdout.includes(`B ${target}\n`)) {
        killed = true;
        const deadline = performance.now() + delayMicros / 1000;
        while (performance.now() < deadline) {
          // Spin: the kill point is this many microseconds into the operation.
        }
        child.kill('SIGKILL');
      }
    });
    child.stderr.on('data', (chunk) => {
      stderr += chunk;
    });
    child.on('error', reject);
    child.on('close', (code, signal) => resolve({ stdout, stderr, code, signal, killed }));
  });
}

function progress(stdout) {
  const begun = [];
  const acknowledged = [];
  for (const line of stdout.split('\n')) {
    const [marker, index] = line.split(' ');
    if (marker === 'B') begun.push(Number(index));
    if (marker === 'A') acknowledged.push(Number(index));
  }
  return { begun, acknowledged, done: stdout.includes('DONE\n') };
}

function modelThrough(operations, count) {
  const model = new Map();
  for (const operation of operations.slice(0, count)) apply(model, operation);
  return model;
}

function storedDocuments(store) {
  const documents = new Map();
  let cursor;
  do {
    const page = store.scan({ limit: 64, fields: { text: true }, cursor });
    for (const document of page.documents) {
      documents.set(Number(document.id), { revision: Number(document.revision), text: document.text });
    }
    cursor = page.cursor ?? undefined;
  } while (cursor !== undefined);
  return documents;
}

function sameState(left, right) {
  if (left.size !== right.size) return false;
  for (const [id, value] of left) {
    const other = right.get(id);
    if (other === undefined || other.revision !== value.revision || other.text !== value.text) {
      return false;
    }
  }
  return true;
}

function describe(map) {
  return JSON.stringify([...map].sort(([left], [right]) => left - right));
}

test('a killed writer leaves a verified store holding every acknowledged write', async () => {
  const seed = parseSeed(process.env.ZE_TEST_SEED);
  const rounds = parseRounds(process.env.ZE_CRASH_ROUNDS);
  const results = [];
  console.log(`crash-safety seed=${seed} rounds=${rounds} (replay with ZE_TEST_SEED=${seed})`);
  for (let round = 0; round < rounds; round += 1) {
    const roundSeed = (seed + Math.imul(round, 0x9e37_79b9)) >>> 0;
    const operations = workload(roundSeed, OPERATIONS);
    // Rotate the targeted operation type so every round set kills during
    // upserts, deletes and seals, then pick one such operation by seed.
    const type = TYPES[round % TYPES.length];
    const candidates = operations
      .map((operation, index) => ({ operation, index }))
      .filter(({ operation }) => operation.type === type);
    assert.ok(candidates.length > 0, `round ${round}: workload has no ${type}`);
    const pick = rng(roundSeed ^ 0x5eed);
    const target = candidates[Math.floor(pick() * candidates.length)].index;
    const delayMicros = Math.floor(pick() * 3000);

    const root = mkdtempSync(join(tmpdir(), 'zeppelin-node-crash-'));
    const context = `round ${round} seed ${roundSeed} target ${target} (${type}) +${delayMicros}us`;
    try {
      const run = await runUntilKilled(root, roundSeed, target, delayMicros);
      assert.equal(run.killed, true, `${context}: child never reached the target\n${run.stderr}`);
      assert.equal(run.stderr, '', `${context}: child wrote to stderr`);
      const { begun, acknowledged, done } = progress(run.stdout);
      const acked = acknowledged.length;
      assert.deepEqual(
        acknowledged,
        [...Array(acked).keys()],
        `${context}: acknowledgements are not a prefix`,
      );
      const inFlight = begun.length > acked ? acked : null;
      assert.ok(begun.length - acked <= 1, `${context}: more than one operation in flight`);

      const directory = join(root, NAMESPACE);
      const report = verify(directory);
      assert.deepEqual(
        report.findings,
        [],
        `${context}: verify found damage after the kill`,
      );

      const store = openNamespace(root, NAMESPACE, SPEC, OPTIONS);
      let outcome;
      try {
        const stored = storedDocuments(store);
        const before = modelThrough(operations, acked);
        if (sameState(stored, before)) {
          outcome = inFlight === null ? 'acknowledged' : 'inFlightAbsent';
        } else {
          assert.notEqual(
            inFlight,
            null,
            `${context}: store differs from the acknowledged writes\nstored ${describe(stored)}\nexpected ${describe(before)}`,
          );
          const after = modelThrough(operations, acked + 1);
          assert.ok(
            sameState(stored, after),
            `${context}: the in-flight ${operations[inFlight].type} is partly applied\nstored ${describe(stored)}\nbefore ${describe(before)}\nafter ${describe(after)}`,
          );
          outcome = 'inFlightPresent';
        }
        // The recovered store accepts writes. It does not seal again: a second
        // seal can meet ZE-236 (see crash-workload.cjs).
        store.upsert([
          { id: 1_000_000n, text: 'after recovery', attributes: [{ id: 1, type: 'u64', value: 0n }] },
        ]);
      } finally {
        store.close();
      }
      assert.deepEqual(verify(directory).findings, [], `${context}: damage after recovery writes`);
      const reopened = openNamespace(root, NAMESPACE, SPEC, OPTIONS);
      try {
        assert.equal(
          reopened.get([1_000_000n]).missingCount,
          0,
          `${context}: the write after recovery did not survive a reopen`,
        );
      } finally {
        reopened.close();
      }
      results.push({
        round,
        seed: roundSeed,
        target,
        targetType: type,
        delayMicros,
        childFinished: done,
        acknowledged: acked,
        killedDuring: inFlight === null ? null : operations[inFlight].type,
        outcome,
        signal: run.signal,
        exitCode: run.code,
        walRecordsChecked: Number(report.walRecordsChecked),
        segmentsChecked: Number(report.segmentsChecked),
      });
    } finally {
      rmSync(root, { recursive: true, force: true });
    }
  }

  const evidence = process.env.ZE_CRASH_EVIDENCE;
  if (evidence) {
    writeFileSync(
      evidence,
      `${JSON.stringify(
        {
          suite: 'bindings/node/test/crash-safety.test.mjs',
          seed,
          rounds,
          operationsPerRound: OPERATIONS,
          platform: process.platform,
          arch: process.arch,
          node: process.version,
          options: OPTIONS,
          results,
        },
        null,
        2,
      )}\n`,
    );
  }
  const counts = results.reduce((total, result) => {
    total[result.outcome] = (total[result.outcome] ?? 0) + 1;
    return total;
  }, {});
  console.log(`crash-safety outcomes ${JSON.stringify(counts)}`);
});
