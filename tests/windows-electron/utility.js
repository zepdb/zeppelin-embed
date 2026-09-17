'use strict';

/**
 * The utility process. All native work happens here, never in the main
 * process, which is the arrangement a real Electron application uses: a crash
 * or a blocking call in the engine must not take the UI process with it.
 *
 * It speaks a tiny request/response protocol over the parent port. Every reply
 * carries either `ok: true` with a result or `ok: false` with the error's
 * name, message and Zeppelin error code, so the main process can assert on
 * typed failures rather than on message text.
 */

const fs = require('node:fs');
const os = require('node:os');
const path = require('node:path');

/** Stores held open by this process, keyed by the id the parent gave them. */
const open = new Map();

let zeppelin;
let loadError;
try {
  // eslint-disable-next-line global-require
  zeppelin = require('@zepdb/zeppelin-embed');
} catch (error) {
  loadError = error;
}

function describeError(error) {
  return {
    name: error?.name ?? 'Error',
    message: error?.message ?? String(error),
    code: error?.code ?? null,
    errorCode: typeof error?.errorCode === 'number' ? error.errorCode : null,
  };
}

/**
 * Where the addon was actually loaded from, so the test can prove the loader
 * selected the Electron binary rather than the plain Node one.
 */
function resolvedAddon() {
  const loaded = Object.keys(require.cache).filter((file) => file.endsWith('.node'));
  return loaded.length > 0 ? loaded : null;
}

function vector(seed) {
  return Float32Array.from([seed, seed + 1, seed + 2, seed + 3]);
}

const handlers = {
  /** Reports the runtime, so the host is identified from inside it. */
  describe() {
    return {
      electron: process.versions.electron ?? null,
      node: process.versions.node,
      napi: process.versions.napi ?? null,
      platform: process.platform,
      arch: process.arch,
      abiVersion: zeppelin.ABI_VERSION,
      addons: resolvedAddon(),
      execPath: process.execPath,
      resourcesPath: process.resourcesPath ?? null,
    };
  },

  /** Opens a store and keeps it open under `id`. */
  open({ id, directory }) {
    fs.mkdirSync(directory, { recursive: true });
    open.set(id, new zeppelin.Store(directory));
    return { opened: id };
  },

  /** A full lifecycle: upsert, get, vector search. */
  exercise({ id }) {
    const store = open.get(id);
    if (!store) throw new Error(`no store open under ${id}`);

    const WIDE = (1n << 64n) | 1n;
    const corpus = [
      { id: 1n, revision: 1n, vector: vector(1) },
      { id: 2n, revision: 1n, vector: vector(5) },
      // A full-width id whose low 64 bits collide with id 1, to prove the
      // bigint path survives the IPC and the addon boundary.
      { id: WIDE, revision: 1n, vector: vector(9) },
    ];
    store.upsert(corpus);

    const fetched = store.get([1n, WIDE, 4242n]);

    const query = vector(5);
    const hits = store.search(query, 2);

    // Rank the same corpus independently and compare. The engine's Node-level
    // score is an inner product, larger-is-better, so the expected order is by
    // descending dot product with ties broken by ascending id. This is
    // recomputed here rather than assumed from the returned scores, so a
    // scoring or ordering change would fail rather than be absorbed.
    //
    // It deliberately does not assert that the vector equal to the query ranks
    // first: that is only true for a distance metric or normalised inputs, and
    // these vectors are neither.
    const expected = corpus
      .map((document) => {
        let dot = 0;
        for (let index = 0; index < query.length; index += 1) {
          dot += query[index] * document.vector[index];
        }
        return { id: document.id, dot };
      })
      .sort((left, right) => (right.dot - left.dot) || (left.id < right.id ? -1 : 1))
      .slice(0, 2)
      .map((entry) => entry.id.toString());

    return {
      documentCount: fetched.documents.filter(Boolean).length,
      missing: fetched.documents.filter((document) => !document).length,
      ids: hits.map((hit) => hit.id.toString()),
      expectedIds: expected,
      rankingMatchesIndependentScoring:
        hits.length === expected.length &&
        hits.every((hit, index) => hit.id.toString() === expected[index]),
      wideIdRoundTripped: fetched.documents.some(
        (document) => document && document.id === WIDE,
      ),
    };
  },

  close({ id }) {
    const store = open.get(id);
    if (!store) throw new Error(`no store open under ${id}`);
    store.close();
    open.delete(id);
    return { closed: id };
  },

  /** Exits abruptly, so the parent can prove ownership is released on death. */
  die() {
    process.exit(9);
  },
};

process.parentPort.on('message', (event) => {
  const request = event.data;
  let reply;
  if (loadError) {
    reply = { id: request.id, ok: false, error: describeError(loadError) };
  } else {
    try {
      const handler = handlers[request.kind];
      if (!handler) throw new Error(`unknown request kind ${request.kind}`);
      reply = { id: request.id, ok: true, result: handler(request.payload ?? {}) };
    } catch (error) {
      reply = { id: request.id, ok: false, error: describeError(error) };
    }
  }
  process.parentPort.postMessage(reply);
});
