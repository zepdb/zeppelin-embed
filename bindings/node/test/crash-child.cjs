'use strict';

// Child half of the crash-safety suite. Runs the seeded workload against one
// namespace and reports progress on stdout with synchronous writes, so every
// line written before the parent kills this process reaches the pipe:
//   B <index>  the operation is about to start
//   A <index>  the operation returned, so the write is acknowledged
//   DONE       every operation returned

const { writeSync } = require('node:fs');
const { openNamespace } = require('..');
const { NAMESPACE, OPTIONS, SPEC, workload } = require('./crash-workload.cjs');

const [root, seedText, countText] = process.argv.slice(2);
const operations = workload(Number(seedText), Number(countText));
const store = openNamespace(root, NAMESPACE, SPEC, OPTIONS);

for (const [index, operation] of operations.entries()) {
  writeSync(1, `B ${index}\n`);
  if (operation.type === 'upsert') {
    store.upsert(
      operation.documents.map((document) => ({
        id: BigInt(document.id),
        revision: BigInt(document.revision),
        text: document.text,
        attributes: [{ id: 1, type: 'u64', value: BigInt(document.round) }],
      })),
    );
  } else if (operation.type === 'delete') {
    store.delete(operation.ids.map((id) => BigInt(id)));
  } else {
    store.seal();
  }
  writeSync(1, `A ${index}\n`);
}
writeSync(1, 'DONE\n');
store.close();
