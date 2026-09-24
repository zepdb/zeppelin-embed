import assert from 'node:assert/strict';
import { spawn } from 'node:child_process';
import { mkdtempSync, readFileSync, rmSync, statSync, writeFileSync } from 'node:fs';
import { createRequire } from 'node:module';
import { tmpdir } from 'node:os';
import { join } from 'node:path';
import { fileURLToPath } from 'node:url';
import test from 'node:test';

const require = createRequire(import.meta.url);
const { openNamespace } = require('..');
const { NAMESPACE, OPTIONS, SPEC, batchAt, metadataFor, oracleAfter } = require(
  './batch-atomicity-child.cjs',
);

const CHILD = fileURLToPath(new URL('./batch-atomicity-child.cjs', import.meta.url));
const ITERATIONS = Number(process.env.ZE_BATCH_KILL_ITERS ?? 16);
const SEED = Number(process.env.ZE_TEST_SEED ?? 0x216) >>> 0;
const DEADLINE_MS = 30000;
// Each store takes this many kills before a fresh one starts, which keeps
// the unsealed WAL small while still reopening over earlier aborted batches.
const KILLS_PER_STORE = 4;

/** Deterministic parent-side choices, separate from the workload stream. */
function parentRandom(seed) {
  let state = (seed ^ 0x5bd1e995) >>> 0 || 1;
  return () => {
    state = (Math.imul(state, 1664525) + 1013904223) >>> 0;
    return state / 0x100000000;
  };
}

function walBytes(root) {
  try {
    return statSync(join(root, NAMESPACE, 'wal.ze')).size;
  } catch (error) {
    if (error.code === 'ENOENT') return 0;
    throw error;
  }
}

function sleep(milliseconds) {
  return new Promise((resolve) => setTimeout(resolve, milliseconds));
}

/** Reads every live document as id -> { revision, metadata }. */
function recoveredState(root) {
  const store = openNamespace(root, NAMESPACE, SPEC, OPTIONS);
  try {
    const state = new Map();
    let cursor;
    do {
      const page = store.scan({ cursor, limit: 1000, fields: { metadata: true } });
      for (const document of page.documents) {
        state.set(Number(document.id), document);
      }
      cursor = page.cursor ?? undefined;
    } while (cursor !== undefined);
    assert.equal(store.count().count, BigInt(state.size), 'count agrees with scan');
    return state;
  } finally {
    store.close();
  }
}

/** True when the recovered documents are exactly the oracle after `count` batches. */
function matchesOracle(recovered, count) {
  const oracle = oracleAfter(SEED, count);
  if (oracle.size !== recovered.size) return false;
  for (const [id, batch] of oracle) {
    const document = recovered.get(id);
    if (document === undefined || document.revision !== BigInt(batch + 1)) return false;
    const expected = metadataFor(batch, id, batchAt(SEED, batch).metadataBytes);
    if (Buffer.compare(Buffer.from(document.metadata), Buffer.from(expected)) !== 0) {
      return false;
    }
  }
  return true;
}

function describe(recovered) {
  return [...recovered]
    .sort(([left], [right]) => left - right)
    .map(([id, document]) => `${id}@${document.revision}`)
    .join(' ');
}

async function runChild(root, first, progress, killAt) {
  writeFileSync(progress, '');
  const child = spawn(process.execPath, [CHILD, root, String(SEED), String(first), progress], {
    stdio: ['ignore', 'pipe', 'pipe'],
  });
  let stderr = '';
  child.stderr.on('data', (chunk) => {
    stderr += chunk;
  });
  const exited = new Promise((resolve) => child.once('exit', (code, signal) => resolve({ code, signal })));
  await new Promise((resolve, reject) => {
    child.stdout.once('data', resolve);
    child.once('exit', () => reject(new Error(`child exited before ready: ${stderr}`)));
  });
  const started = Date.now();
  if (killAt.kind === 'delay') {
    await sleep(killAt.milliseconds);
  } else {
    // Kill as soon as the WAL crosses a random byte position, which lands
    // inside a multi-group batch far more often than a timer does.
    const threshold = walBytes(root) + killAt.bytes;
    while (walBytes(root) < threshold) {
      if (Date.now() - started > DEADLINE_MS) break;
    }
  }
  child.kill('SIGKILL');
  const status = await exited;
  assert.equal(status.signal, 'SIGKILL', `child died on its own: ${stderr}`);
  const lines = readFileSync(progress, 'utf8').split('\n');
  // Only newline-terminated lines are acknowledgements; a kill can cut the last.
  const acknowledged = lines.slice(0, -1).map(Number);
  assert.deepEqual(
    acknowledged,
    acknowledged.map((_, index) => first + index),
    'the child acknowledged batches in order',
  );
  return acknowledged.length;
}

test('a SIGKILLed batch is recovered whole or not at all', async () => {
  const random = parentRandom(SEED);
  // `tornCut` counts reopens that cut an interrupted final append.
  const outcomes = { absent: 0, visible: 0, tornCut: 0, batches: 0 };
  let root;
  try {
    let recoveredBatches = 0;
    for (let iteration = 0; iteration < ITERATIONS; iteration += 1) {
      if (iteration % KILLS_PER_STORE === 0) {
        if (root !== undefined) rmSync(root, { recursive: true, force: true });
        root = mkdtempSync(join(tmpdir(), 'ze-batch-atomicity-'));
        outcomes.batches += recoveredBatches;
        recoveredBatches = 0;
      }
      const killAt =
        iteration % 2 === 0
          ? { kind: 'delay', milliseconds: 20 + Math.floor(random() * 250) }
          : { kind: 'walBytes', bytes: 1 + Math.floor(random() * 4 * 1024 * 1024) };
      const first = recoveredBatches;
      const acknowledged = await runChild(root, first, join(root, 'progress'), killAt);
      const killedBytes = walBytes(root);
      const recovered = recoveredState(root);
      if (walBytes(root) < killedBytes) outcomes.tornCut += 1;
      // Every acknowledged batch is visible, and the one batch in flight at
      // the kill is either wholly visible or wholly absent: no other state.
      if (matchesOracle(recovered, first + acknowledged)) {
        recoveredBatches = first + acknowledged;
        outcomes.absent += 1;
      } else if (matchesOracle(recovered, first + acknowledged + 1)) {
        recoveredBatches = first + acknowledged + 1;
        outcomes.visible += 1;
      } else {
        assert.fail(
          `iteration ${iteration} seed ${SEED} ${JSON.stringify(killAt)}: recovered ` +
            `state is neither ${first + acknowledged} nor ${first + acknowledged + 1} ` +
            `whole batches: ${describe(recovered)}`,
        );
      }
    }
    outcomes.batches += recoveredBatches;
    console.log(`batch atomicity seed=${SEED} iterations=${ITERATIONS} ${JSON.stringify(outcomes)}`);
    assert.ok(outcomes.batches > 0, 'the children committed batches');
  } finally {
    if (root !== undefined) rmSync(root, { recursive: true, force: true });
  }
});
