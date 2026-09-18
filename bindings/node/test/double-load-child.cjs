'use strict';

const { Worker, isMainThread, workerData } = require('node:worker_threads');
const { resolve } = require('node:path');
const {
  doubleLoad,
  collectAndReopen,
  leaveForExit,
} = require('./double-load.cjs');

const packagePath = resolve(__dirname, '..');
async function run() {
  if (!isMainThread) {
    doubleLoad(packagePath, workerData.root);
    leaveForExit(packagePath, workerData.root);
    return;
  }
  const [mode, root] = process.argv.slice(2);
  if (mode === 'worker') {
    const api = require(packagePath);
    const keeper = api.openNamespace(resolve(root, 'main'), 'keeper', {});
    keeper.upsert([{ id: 9n, text: 'main survives worker teardown' }]);
    try {
      await new Promise((resolveWorker, reject) => {
        const worker = new Worker(__filename, {
          workerData: { root },
          execArgv: [],
        });
        worker.once('error', reject);
        worker.once('exit', (code) =>
          code === 0
            ? resolveWorker()
            : reject(new Error(`worker exit ${code}`)),
        );
      });
      const reopened = api.openNamespace(resolve(root, 'exit'), 'unclosed', {});
      try {
        require('node:assert/strict').equal(
          reopened.get([4n]).documents[0].text,
          'teardown',
        );
      } finally {
        reopened.close();
      }
    } finally {
      require('node:assert/strict').equal(
        keeper.get([9n]).documents[0].text,
        'main survives worker teardown',
      );
      keeper.close();
    }
  } else {
    console.log(JSON.stringify(doubleLoad(packagePath, root)));
    await collectAndReopen(packagePath, root);
    leaveForExit(packagePath, root);
  }
  console.log(`WF-137 ${mode} passed`);
}
run().catch((error) => {
  console.error(error);
  process.exitCode = 1;
});
