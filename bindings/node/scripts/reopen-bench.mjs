// Reopen and append benchmark for text-indexed namespaces (ZE-231, ZE-232).
//
// Writes `docs` small texted documents, closes, and times the reopen and the
// first text query. Prints one JSON line per size.
//
//   node scripts/reopen-bench.mjs --docs 3000,20000,100000 [--auto-seal-rows 2048]
//     [--batch 1] [--max-resident-bytes 536870912]
//     [--package <path to a zeppelin-embed package>]
//
// `--batch` writes that many documents per upsert. Without `--auto-seal-rows`
// nothing is sealed, which measures WAL replay; `--package` measures another
// build, such as a published release, with the same workload.
import { mkdtempSync, readdirSync, rmSync, statSync } from 'node:fs';
import { createRequire } from 'node:module';
import { tmpdir } from 'node:os';
import { join, resolve } from 'node:path';
import { performance } from 'node:perf_hooks';
import { parseArgs } from 'node:util';
import { fileURLToPath } from 'node:url';

const { values } = parseArgs({
  options: {
    docs: { type: 'string', default: '3000,20000' },
    'auto-seal-rows': { type: 'string' },
    batch: { type: 'string', default: '1' },
    package: { type: 'string' },
    'max-resident-bytes': { type: 'string' },
  },
});

const require = createRequire(import.meta.url);
const packagePath = values.package
  ? resolve(values.package)
  : fileURLToPath(new URL('..', import.meta.url));
const { openNamespace } = require(packagePath);

const sizes = values.docs.split(',').map((value) => Number.parseInt(value, 10));
const batch = Number.parseInt(values.batch, 10);
const autoSealRows =
  values['auto-seal-rows'] === undefined
    ? undefined
    : Number.parseInt(values['auto-seal-rows'], 10);
const options = {
  durability: 'durable',
  commitTier: 'none',
  ...(autoSealRows === undefined ? {} : { autoSealRows }),
  ...(values['max-resident-bytes'] === undefined
    ? {}
    : { maxResidentBytes: BigInt(values['max-resident-bytes']) }),
};
const spec = { attributes: [{ id: 1, name: 'note', type: 'rawString' }] };

function text(index) {
  return `segment ${index} the quick brown fox jumps over the lazy dog ${index % 97}`;
}

function directoryBytes(path) {
  return readdirSync(path).reduce((total, file) => total + statSync(join(path, file)).size, 0);
}

for (const size of sizes) {
  const root = mkdtempSync(join(tmpdir(), 'zeppelin-reopen-bench-'));
  try {
    let store = openNamespace(root, 'segments', spec, options);
    // Mean append latency over each tenth of the load, in milliseconds.
    const appendMs = [];
    const window = Math.max(1, Math.floor(size / 10));
    let windowStart = performance.now();
    let written = 0;
    while (written < size) {
      const documents = [];
      for (let index = 0; index < batch && written < size; index += 1) {
        written += 1;
        documents.push({
          id: BigInt(written),
          text: text(written),
          attributes: [{ id: 1, type: 'string', value: `note-${written % 50}` }],
        });
      }
      store.upsert(documents);
      if (written % window === 0) {
        const now = performance.now();
        appendMs.push(Number(((now - windowStart) / window).toFixed(3)));
        windowStart = now;
      }
    }
    store.close();

    const started = performance.now();
    store = openNamespace(root, 'segments', spec, options);
    const reopenMs = performance.now() - started;
    const queryStarted = performance.now();
    const hits = store.query({ text: 'fox', k: 10 }).hits.length;
    const firstQueryMs = performance.now() - queryStarted;
    const count = store.count().count;
    store.close();

    const namespace = join(root, 'segments');
    console.log(
      JSON.stringify({
        docs: size,
        batch,
        autoSealRows: autoSealRows ?? null,
        reopenMs: Number(reopenMs.toFixed(1)),
        firstQueryMs: Number(firstQueryMs.toFixed(1)),
        appendMsPerDocByTenth: appendMs,
        segments: readdirSync(namespace).filter((file) => file.endsWith('.zseg')).length,
        walBytes: statSync(join(namespace, 'wal.ze')).size,
        storeBytes: directoryBytes(namespace),
        count: Number(count),
        hits,
      }),
    );
  } finally {
    rmSync(root, { force: true, recursive: true });
  }
}
