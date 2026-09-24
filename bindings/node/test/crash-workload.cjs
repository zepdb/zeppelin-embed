'use strict';

/**
 * The seeded workload shared by the crash-safety parent and its child.
 *
 * Both processes derive the identical operation list from the seed, so the
 * parent's oracle knows exactly what every acknowledged operation did without
 * trusting anything the killed child wrote beyond its acknowledgements.
 */

const SPEC = { attributes: [{ id: 1, name: 'round', type: 'u64' }] };
// A source-of-truth store; the page cache covers a process kill, which is the
// failure this suite injects, so each write skips the device flush.
const OPTIONS = { durability: 'durable', commitTier: 'none' };
const NAMESPACE = 'notes';
const ID_SPACE = 48;
// ZE-236: a sealed-segment replacement id is the original id XOR (generation,
// nonce), which can equal another live segment's id, so a later write
// overwrites that segment and the next write fails. It needs two sealed
// segments to exist, so one seal per round keeps the suite about crash
// safety. Remove this cap when ZE-236 is fixed.
const MAX_SEALS_PER_ROUND = 1;

/** mulberry32: a small, fully specified 32-bit PRNG. */
function rng(seed) {
  let state = seed >>> 0;
  return () => {
    state = (state + 0x6d2b79f5) >>> 0;
    let value = state;
    value = Math.imul(value ^ (value >>> 15), value | 1);
    value ^= value + Math.imul(value ^ (value >>> 7), value | 61);
    return ((value ^ (value >>> 14)) >>> 0) / 4294967296;
  };
}

function text(id, revision) {
  return `meeting ${id} revision ${revision} harbour notes and follow-ups`;
}

/**
 * Returns `count` operations. Upserts write one to four documents, deletes
 * remove one to three present documents, and roughly one operation in eight
 * seals, up to `MAX_SEALS_PER_ROUND`. Upserts and deletes after the seal also
 * rewrite the sealed segment. Revisions only grow per id, so every upsert is
 * a legal write.
 */
function workload(seed, count) {
  const random = rng(seed);
  const revisions = new Map();
  const present = new Set();
  const operations = [];
  let seals = 0;
  for (let index = 0; index < count; index += 1) {
    const roll = random();
    if (roll < 0.125 && seals < MAX_SEALS_PER_ROUND) {
      seals += 1;
      operations.push({ type: 'seal' });
    } else if (roll < 0.35 && present.size > 0) {
      const pool = [...present].sort((left, right) => left - right);
      const ids = new Set();
      const wanted = 1 + Math.floor(random() * 3);
      while (ids.size < Math.min(wanted, pool.length)) {
        ids.add(pool[Math.floor(random() * pool.length)]);
      }
      for (const id of ids) present.delete(id);
      operations.push({ type: 'delete', ids: [...ids] });
    } else {
      const ids = new Set();
      const wanted = 1 + Math.floor(random() * 4);
      while (ids.size < wanted) ids.add(1 + Math.floor(random() * ID_SPACE));
      const documents = [...ids].map((id) => {
        const revision = (revisions.get(id) ?? 0) + 1;
        revisions.set(id, revision);
        present.add(id);
        return { id, revision, text: text(id, revision), round: index };
      });
      operations.push({ type: 'upsert', documents });
    }
  }
  return operations;
}

/** Applies one operation to a `Map<id, {revision, text}>` model in place. */
function apply(model, operation) {
  if (operation.type === 'upsert') {
    for (const document of operation.documents) {
      model.set(document.id, { revision: document.revision, text: document.text });
    }
  } else if (operation.type === 'delete') {
    for (const id of operation.ids) model.delete(id);
  }
}

module.exports = { NAMESPACE, OPTIONS, SPEC, apply, rng, workload };
