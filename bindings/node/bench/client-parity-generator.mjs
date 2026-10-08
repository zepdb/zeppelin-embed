// Synthetic corpus generated from ADR-017 assumptions, NOT the client's recorded queries:
// replace or extend with the client's real corpus when provided.
// No dependencies. Stable xorshift32; document identities depend only on ordinal.
export const provenance = "synthetic corpus generated from ADR-017 assumptions, NOT the client's recorded queries: replace or extend with the client's real corpus when provided";
export const seed = 386;
export const dimensions = 8; // Synthetic: ADR-017 specifies no embedding dimension.
export const SPEC = {
  vectorSpace: { dimensions },
  attributes: ['note', 'folder', 'speaker', 'stream', 'startMs', 'endMs']
    .map((name, i) => ({ id: i + 1, name, type: 'u64', nullable: false })),
};
export function rng(initial = seed) {
  let state = initial >>> 0;
  return () => {
    state ^= state << 13; state ^= state >>> 17; state ^= state << 5;
    return (state >>> 0) / 4294967296;
  };
}
const vocabulary = 'budget scope design review launch issue owner check draft goals notes tasks risks teams plans dates costs tests users ideas build sales calls fixes'.split(' ');
export const topics = ['harbour', 'orchard', 'release', 'meeting', 'project', 'café', '東京', 'roadmap', 'service', 'storage'];
export function vector(random) {
  return Array.from({ length: dimensions }, () => Math.fround(random() * 2 - 1));
}
// One year = 500 meetings * (300 segments + note + summary).
// Five years is supported for generation, but parity records one year only.
export function* documentBatches(years = 1) {
  if (![1, 5].includes(years)) throw new Error('years must be 1 or 5');
  const random = rng();
  for (let note = 0; note < 500 * years; note++) {
    const batch = [];
    for (let segment = 0; segment < 302; segment++) {
      // Paired lengths 20..40 average exactly 30 over the 300 transcript rows.
      const length = segment < 300 ? 20 + (segment % 2 ? 20 - Math.floor(segment / 2) % 21 : Math.floor(segment / 2) % 21) : 60;
      const words = [topics[note % topics.length], `plan${segment % 64}`, '🚀'];
      while (words.length < length) words.push(vocabulary[Math.floor(random() * vocabulary.length)]);
      const start = Math.min(segment, 299) * 12000;
      batch.push({
        id: BigInt(note * 302 + segment + 1), revision: 1n,
        timestamp: BigInt(1700000000000 + note * 86400000 + start),
        text: words.join(' '), vector: new Float32Array(vector(random)),
        attributes: [note, note % 10, segment % 4, segment < 300 ? 0 : 1, start, start + 12000]
          .map((value, i) => ({ id: i + 1, type: 'u64', value: BigInt(value) })),
      });
    }
    yield batch;
  }
}
export function queryShapes() {
  const random = rng(seed ^ 0x51554552);
  return Array.from({ length: 500 }, (_, i) => {
    const group = Math.floor(i / 100), note = Math.floor(random() * 500);
    const text = topics[note % topics.length];
    const request = { text, k: 20 };
    if (group === 0) request.snippetBytes = 300;
    if (group === 1) {
      request.text = i % 2 ? `${text} pla` : text.slice(0, -1);
      request.lastAsPrefix = true;
    }
    if (group === 2) {
      request.filter = { op: 'in', attributeId: 2,
        values: [note % 10, (note + 1) % 10].map(value => ({ id: 2, type: 'u64', value: String(value) })) };
      if (i % 2 === 0) request.eligibleIds = Array.from({ length: 302 }, (_, j) => String(note * 302 + j + 1));
    }
    if (group === 3) {
      request.vector = vector(random); request.alpha = [0, 0.25, 0.5, 0.75, 1][i % 5];
      request.tier = 'exact'; request.snippetBytes = 300;
    }
    if (group === 4) {
      request.snippetBytes = [32, 64, 128, 300][i % 4];
      if (i % 2) request.lastAsPrefix = true;
    }
    return { name: `${['home', 'type-ahead', 'folder', 'chat', 'citation'][group]}-${i % 100}`, request };
  });
}
export function decodeFilter(filter) {
  return { ...filter,
    ...(filter.children ? { children: filter.children.map(decodeFilter) } : {}),
    ...(filter.values ? { values: filter.values.map(value => ({ ...value, value: BigInt(value.value) })) } : {}),
  };
}
export function decodeRequest(request) {
  return { ...request,
    ...(request.vector ? { vector: new Float32Array(request.vector) } : {}),
    ...(request.eligibleIds ? { eligibleIds: request.eligibleIds.map(BigInt) } : {}),
    ...(request.filter ? { filter: decodeFilter(request.filter) } : {}),
  };
}
