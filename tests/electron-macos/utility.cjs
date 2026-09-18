'use strict';

const assert = require('node:assert/strict');
const { doubleLoad, leaveForExit } = require('./double-load.cjs');
assert.equal(process.type, 'utility');
assert.equal(process.versions.electron, '44.4.1');
process.parentPort.once('message', ({ data }) => {
  const result = doubleLoad('@zepdb/zeppelin-embed', data.root);
  leaveForExit('@zepdb/zeppelin-embed', data.root);
  process.parentPort.once('message', ({ data: shutdown }) => {
    assert.equal(shutdown, 'shutdown');
  });
  process.parentPort.postMessage(result);
  // No explicit process.exit: the parent must observe normal native teardown.
});
