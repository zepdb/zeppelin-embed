'use strict';

const assert = require('node:assert/strict');
const { app, utilityProcess } = require('electron');
const { join } = require('node:path');
const { mkdirSync, writeFileSync } = require('node:fs');
const { doubleLoad } = require('./double-load.cjs');

const dataRoot = process.env.ZE_FIXTURE_ROOT;
assert.ok(dataRoot);
const userData = join(dataRoot, 'electron-user-data');
mkdirSync(userData, { recursive: true });
app.setPath('userData', userData);

const children = new Set();
const timer = setTimeout(() => fail(new Error('fixture timeout')), 30000);
function fail(error) {
  console.error(error);
  for (const child of children) child.kill();
  app.exit(1);
}
function utility(root) {
  const child = utilityProcess.fork(join(__dirname, 'utility.cjs'), [], {
    stdio: 'inherit',
  });
  children.add(child);
  let report;
  let readyResolve;
  let readyReject;
  const ready = new Promise((resolve, reject) => {
    readyResolve = resolve;
    readyReject = reject;
  });
  child.once('message', (message) => {
    report = message;
    readyResolve(message);
  });
  const exited = new Promise((resolve, reject) => {
    child.once('error', (...detail) => {
      const error = new Error(JSON.stringify(detail));
      readyReject(error);
      reject(error);
    });
    child.once('exit', (code) => {
      children.delete(child);
      try {
        assert.equal(code, 0, 'utility exited cleanly');
        assert.equal(report?.nativeLoads, 2);
        resolve();
      } catch (error) {
        readyReject(error);
        reject(error);
      }
    });
  });
  // Observe failures even while waiting at the readiness barrier.
  exited.catch(fail);
  child.postMessage({ root });
  return { child, ready, exited };
}
app
  .whenReady()
  .then(async () => {
    assert.equal(app.isPackaged, true);
    assert.equal(process.versions.electron, '44.4.1');
    const root = process.env.ZE_FIXTURE_ROOT;
    const mode = process.env.ZE_FIXTURE_MODE;
    assert.ok(root);
    assert.ok(['single', 'concurrent'].includes(mode));
    const main = doubleLoad('@zepdb/zeppelin-embed', join(root, 'main'));
    const count = mode === 'concurrent' ? 2 : 1;
    const workers = Array.from({ length: count }, (_, i) =>
      utility(join(root, `utility-${i}`)),
    );
    const reports = await Promise.all(workers.map((worker) => worker.ready));
    // Both utility processes hold different native stores at this barrier.
    assert.ok(
      workers.every(
        ({ child }) => children.has(child) && child.pid !== undefined,
      ),
      'utilities remain alive together',
    );
    for (const worker of workers) worker.child.postMessage('shutdown');
    await Promise.all(workers.map((worker) => worker.exited));
    assert.equal(
      new Set([main.pid, ...reports.map((report) => report.pid)]).size,
      count + 1,
    );
    writeFileSync(
      process.env.ZE_FIXTURE_REPORT,
      JSON.stringify(
        {
          packaged: app.isPackaged,
          electron: process.versions.electron,
          arch: process.arch,
          mode,
          main,
          utilities: reports,
        },
        null,
        2,
      ) + '\n',
    );
    clearTimeout(timer);
    app.exit(0);
  })
  .catch(fail);
