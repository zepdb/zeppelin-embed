'use strict';

const assert = require('node:assert/strict');
const { join } = require('node:path');

// Both CommonJS entries are evicted. Counting dlopen and comparing the native
// constructors distinguishes native reinitialization from a cached require.
function doubleLoad(packagePath, root) {
  const entry = require.resolve(packagePath);
  const original = process.dlopen;
  let nativeLoads = 0;
  process.dlopen = function (module, filename, ...args) {
    if (filename.endsWith('zeppelin_embed.node')) nativeLoads += 1;
    return original.call(this, module, filename, ...args);
  };
  const stores = new Set();
  const spec = { vectorSpace: { dimensions: 2 } };
  const open = (api, name) => {
    const store = api.openNamespace(root, name, spec);
    stores.add(store);
    return store;
  };
  const close = (store) => {
    store.close();
    stores.delete(store);
  };
  const write = (store, id, text) => {
    store.upsert([{ id, vector: new Float32Array([1, 0]), text }]);
  };
  const check = (store, id, text) => {
    assert.deepEqual(
      store.get([id]).documents.map((doc) => doc.text),
      [text],
    );
    for (const request of [
      { vector: new Float32Array([1, 0]) },
      { text },
      { text, vector: new Float32Array([1, 0]) },
    ]) {
      assert.deepEqual(
        store.query({ ...request, k: 1 }).hits.map((hit) => hit.id),
        [id],
      );
    }
  };
  try {
    const first = require(entry);
    assert.strictEqual(require(entry), first);
    const addon = Object.keys(require.cache).find(
      (file) =>
        file.startsWith(require('node:path').dirname(entry)) &&
        file.endsWith('.node'),
    );
    assert.ok(addon, 'the package loaded a native addon');
    const nativeFirst = require.cache[addon].exports;
    const a = open(first, 'first');
    write(a, 1n, 'harbour');
    delete require.cache[entry];
    delete require.cache[addon];
    const second = require(entry);
    const nativeSecond = require.cache[addon].exports;
    assert.notStrictEqual(nativeSecond.NativeStore, nativeFirst.NativeStore);
    assert.equal(nativeLoads, 2, 'both loads entered process.dlopen');
    const c = open(second, 'second');
    write(c, 2n, 'airship');
    check(a, 1n, 'harbour');
    check(c, 2n, 'airship');
    close(a);
    assert.throws(() => a.query({ text: 'harbour', k: 1 }), {
      code: 'ZE_ERR_CLOSED',
    });
    check(c, 2n, 'airship');
    close(c);
    // Cross the loader instances only after close: there is one writer per store.
    const reopenedA = open(second, 'first');
    const reopenedC = open(first, 'second');
    check(reopenedA, 1n, 'harbour');
    check(reopenedC, 2n, 'airship');
    close(reopenedA);
    close(reopenedC);
    return { nativeLoads, addon, pid: process.pid, arch: process.arch };
  } finally {
    process.dlopen = original;
    for (const store of stores) store.close();
  }
}

// Verify the native finalizer released the writer lock, rather than merely
// observing a JS WeakRef. Only the Node child runs with --expose-gc.
async function collectAndReopen(packagePath, root) {
  assert.equal(typeof global.gc, 'function');
  const api = require(packagePath);
  (() => {
    const store = api.openNamespace(root, 'collected', {});
    store.upsert([{ id: 3n, text: 'finalized' }]);
  })();
  let reopened;
  for (let attempt = 0; attempt < 100 && !reopened; attempt += 1) {
    global.gc();
    await new Promise((resolve) => setImmediate(resolve));
    try {
      reopened = api.openNamespace(root, 'collected', {});
    } catch (error) {
      if (error.code !== 'ZE_ERR_STORE_BUSY') throw error;
    }
  }
  assert.ok(reopened, 'garbage collection released the native writer');
  try {
    assert.equal(reopened.get([3n]).documents[0].text, 'finalized');
  } finally {
    reopened.close();
  }
}

function leaveForExit(packagePath, root) {
  const api = require(packagePath);
  global.unclosedStore = api.openNamespace(join(root, 'exit'), 'unclosed', {});
  global.unclosedStore.upsert([{ id: 4n, text: 'teardown' }]);
}

module.exports = { doubleLoad, collectAndReopen, leaveForExit };
