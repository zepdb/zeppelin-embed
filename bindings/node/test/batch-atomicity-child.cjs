'use strict';

// The seeded batch workload shared by the SIGKILL parent and its child.
//
// Batch `b` is a pure function of (seed, b), so the parent rebuilds the exact
// oracle state after any number of batches without talking to the child.
// Every upsert carries revision b + 1 and a metadata payload that names its
// batch, so a recovered document says which batch wrote it.

const POOL = 48;
const NAMESPACE = 'notes';
const SPEC = { attributes: [] };
// Ordered syncs a barrier after every WAL group, which widens the window
// between two groups of one large batch that a kill has to land in.
const OPTIONS = { durability: 'durable', commitTier: 'ordered' };

function rng(seed, batch) {
  let state = (seed ^ Math.imul(batch + 1, 0x9e3779b1)) >>> 0 || 1;
  return () => {
    state ^= state << 13;
    state >>>= 0;
    state ^= state >>> 17;
    state ^= state << 5;
    state >>>= 0;
    return state / 0x100000000;
  };
}

function pick(random, count) {
  const ids = Array.from({ length: POOL }, (_, index) => index + 1);
  for (let index = ids.length - 1; index > 0; index -= 1) {
    const other = Math.floor(random() * (index + 1));
    [ids[index], ids[other]] = [ids[other], ids[index]];
  }
  return ids.slice(0, count);
}

/**
 * One batch: `{ kind: 'upsert' | 'delete', ids, metadataBytes }`. About one
 * upsert in four is larger than the 1 MiB WAL group bound, so its records
 * span several appends.
 */
function batchAt(seed, batch) {
  const random = rng(seed, batch);
  if (batch > 0 && random() < 0.25) {
    return { kind: 'delete', ids: pick(random, 1 + Math.floor(random() * 8)) };
  }
  const large = random() < 0.25;
  const count = large ? 20 + Math.floor(random() * 9) : 1 + Math.floor(random() * 12);
  const metadataBytes = large ? 56 * 1024 : 64 + Math.floor(random() * 512);
  return { kind: 'upsert', ids: pick(random, count), metadataBytes };
}

function metadataFor(batch, id, length) {
  const bytes = new Uint8Array(length);
  const view = new DataView(bytes.buffer);
  view.setUint32(0, batch, true);
  view.setUint32(4, id, true);
  for (let index = 8; index < length; index += 1) {
    bytes[index] = (batch + id + index) & 0xff;
  }
  return bytes;
}

/** Oracle: id -> batch that last upserted it, after batches 0..count-1. */
function oracleAfter(seed, count) {
  const state = new Map();
  for (let batch = 0; batch < count; batch += 1) {
    const { kind, ids } = batchAt(seed, batch);
    for (const id of ids) {
      if (kind === 'upsert') state.set(id, batch);
      else state.delete(id);
    }
  }
  return state;
}

function apply(store, seed, batch) {
  const { kind, ids, metadataBytes } = batchAt(seed, batch);
  if (kind === 'delete') {
    store.delete(ids.map(BigInt));
    return;
  }
  store.upsert(
    ids.map((id) => ({
      id: BigInt(id),
      revision: BigInt(batch + 1),
      text: `batch ${batch} note ${id}`,
      metadata: metadataFor(batch, id, metadataBytes),
    })),
  );
}

module.exports = { NAMESPACE, OPTIONS, SPEC, batchAt, metadataFor, oracleAfter };

// Child: node batch-atomicity-child.cjs <root> <seed> <firstBatch> <progress>
// Writes batches from firstBatch on until killed, appending one line per
// acknowledged batch to <progress> after its write returns.
if (require.main === module) {
  const fs = require('node:fs');
  const { openNamespace } = require('..');
  const [root, seedText, firstText, progressPath] = process.argv.slice(2);
  const seed = Number(seedText);
  const store = openNamespace(root, NAMESPACE, SPEC, OPTIONS);
  const progress = fs.openSync(progressPath, 'a');
  process.stdout.write('ready\n');
  for (let batch = Number(firstText); ; batch += 1) {
    apply(store, seed, batch);
    fs.writeSync(progress, `${batch}\n`);
  }
}
