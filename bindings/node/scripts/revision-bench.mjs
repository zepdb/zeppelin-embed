// Revision latency benchmark for text-indexed namespaces (WF-98, ZE-235).
//
// Grows a store of small texted documents through each checkpoint in
// `--docs`. At every checkpoint it writes one new "live note" and revises
// it `--revisions` times at `--rate` revisions a second (the WF-98 editing
// workload), timing each revision, and times the same number of appends of
// new documents. Prints one JSON line per checkpoint.
//
//   node scripts/revision-bench.mjs --docs 1000,5000,20000 [--revisions 40]
//     [--rate 4] [--auto-seal-rows 2048] [--package <path to a package>]
//
// `--rate 0` revises without pausing. `--package` measures another build,
// such as a published release, with the same workload.
import { mkdtempSync, rmSync } from 'node:fs';
import { createRequire } from 'node:module';
import { tmpdir } from 'node:os';
import { join, resolve } from 'node:path';
import { performance } from 'node:perf_hooks';
import { setTimeout as sleep } from 'node:timers/promises';
import { parseArgs } from 'node:util';
import { fileURLToPath } from 'node:url';

const { values } = parseArgs({
  options: {
    docs: { type: 'string', default: '1000,5000,20000' },
    revisions: { type: 'string', default: '40' },
    rate: { type: 'string', default: '4' },
    'auto-seal-rows': { type: 'string' },
    package: { type: 'string' },
  },
});

const require = createRequire(import.meta.url);
const packagePath = values.package
  ? resolve(values.package)
  : fileURLToPath(new URL('..', import.meta.url));
const { openNamespace } = require(packagePath);

const checkpoints = values.docs.split(',').map((value) => Number.parseInt(value, 10));
const revisions = Number.parseInt(values.revisions, 10);
const rate = Number.parseFloat(values.rate);
const autoSealRows =
  values['auto-seal-rows'] === undefined
    ? undefined
    : Number.parseInt(values['auto-seal-rows'], 10);
const options = {
  durability: 'durable',
  commitTier: 'none',
  ...(autoSealRows === undefined ? {} : { autoSealRows }),
};

// About 60 characters, like one line of a meeting note.
function text(index, revision) {
  return `note ${index} revision ${revision} the quick brown fox jumps over ${index % 97}`;
}

function summary(samples) {
  const sorted = [...samples].sort((left, right) => left - right);
  const at = (quantile) =>
    sorted[Math.min(sorted.length - 1, Math.floor(quantile * sorted.length))];
  const round = (value) => Number(value.toFixed(3));
  return { p50: round(at(0.5)), p99: round(at(0.99)), max: round(sorted[sorted.length - 1]) };
}

function timed(operation) {
  const started = performance.now();
  operation();
  return performance.now() - started;
}

const root = mkdtempSync(join(tmpdir(), 'zeppelin-revision-bench-'));
try {
  const store = openNamespace(root, 'notes', {}, options);
  let nextId = 0n;
  const append = () => {
    nextId += 1n;
    store.upsert([{ id: nextId, revision: 1n, text: text(Number(nextId), 1) }]);
  };
  for (const checkpoint of checkpoints) {
    while (nextId < BigInt(checkpoint)) append();

    const appendMs = [];
    for (let index = 0; index < revisions; index += 1) appendMs.push(timed(append));

    nextId += 1n;
    const note = nextId;
    store.upsert([{ id: note, revision: 1n, text: text(Number(note), 1) }]);
    const reviseMs = [];
    for (let revision = 2n; revision < BigInt(revisions) + 2n; revision += 1n) {
      if (rate > 0) await sleep(1000 / rate);
      reviseMs.push(
        timed(() =>
          store.upsert([{ id: note, revision, text: text(Number(note), Number(revision)) }]),
        ),
      );
    }

    console.log(
      JSON.stringify({
        docs: Number(nextId),
        autoSealRows: autoSealRows ?? null,
        rate,
        appendMs: summary(appendMs),
        reviseMs: summary(reviseMs),
      }),
    );
  }
  store.close();
} finally {
  rmSync(root, { force: true, recursive: true });
}
