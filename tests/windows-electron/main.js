'use strict';

/**
 * The Electron main process. It owns no native code: every engine call happens
 * in a utility process, which is what a real application does.
 *
 * It runs the scenario list, writes a JSON report and exits with 0 only if
 * every scenario passed. The report goes to a file named by
 * `ZE_FIXTURE_REPORT` as well as to stdout, because a packaged Windows app is
 * a GUI-subsystem executable and its stdout is not reliably attached to the
 * launching console.
 */

const { app, utilityProcess } = require('electron');
const fs = require('node:fs');
const os = require('node:os');
const path = require('node:path');

const REPORT = process.env.ZE_FIXTURE_REPORT;
const results = [];

function record(name, passed, detail) {
  results.push({ name, passed, detail });
  console.log(`${passed ? 'ok  ' : 'FAIL'} ${name}${detail ? ` :: ${JSON.stringify(detail)}` : ''}`);
}

/** A utility process wrapped in a promise-based request/response client. */
function spawnWorker() {
  const script = path.join(__dirname, 'utility.js');
  const child = utilityProcess.fork(script, [], { stdio: 'inherit' });
  const pending = new Map();
  let nextId = 1;
  let exited = null;

  child.on('message', (message) => {
    const resolve = pending.get(message.id);
    if (resolve) {
      pending.delete(message.id);
      resolve(message);
    }
  });
  const exit = new Promise((resolve) => {
    child.once('exit', (code) => {
      exited = code;
      for (const [, resolvePending] of pending) {
        resolvePending({ ok: false, error: { name: 'ProcessExited', message: `exit ${code}` } });
      }
      pending.clear();
      resolve(code);
    });
  });

  return {
    child,
    exit,
    get exited() {
      return exited;
    },
    /** Fire-and-forget, for requests that deliberately never reply. */
    notify(kind, payload = {}) {
      child.postMessage({ id: 0, kind, payload });
    },

    request(kind, payload = {}, timeoutMs = 30000) {
      const id = nextId;
      nextId += 1;
      return new Promise((resolve) => {
        // A bounded wait. An unbounded one turns any protocol mistake into a
        // hung suite that reports nothing, which is strictly worse than a
        // failure that says which request went unanswered.
        const timer = setTimeout(() => {
          pending.delete(id);
          resolve({
            id,
            ok: false,
            error: { name: 'RequestTimeout', message: `${kind} did not reply within ${timeoutMs}ms` },
          });
        }, timeoutMs);
        pending.set(id, (message) => {
          clearTimeout(timer);
          resolve(message);
        });
        // The payload is nested, never spread into the envelope. Spreading it
        // let a payload field named `id` overwrite the correlation id, so the
        // reply came back under the wrong key and the promise never settled.
        child.postMessage({ id, kind, payload });
      });
    },
    async ready() {
      // `utilityProcess.fork` resolves before the child's listener is attached;
      // the first successful round trip is the real readiness signal.
      for (let attempt = 0; attempt < 100; attempt += 1) {
        const reply = await this.request('describe');
        if (reply.ok) return reply.result;
        if (reply.error?.name === 'ProcessExited') throw new Error(reply.error.message);
        await new Promise((resolve) => setTimeout(resolve, 50));
      }
      throw new Error('utility process never became ready');
    },
  };
}

function scratch(name) {
  return fs.mkdtempSync(path.join(os.tmpdir(), `ze-electron-${name}-`));
}

async function main() {
  const packaged = app.isPackaged;

  // 1. The addon loads inside a real Electron utility process, and the loader
  //    picked the Electron binary rather than the plain Node one.
  const worker = spawnWorker();
  let described;
  try {
    described = await worker.ready();
  } catch (error) {
    record('addon loads in a utility process', false, { error: String(error) });
    return finish(packaged);
  }
  record('addon loads in a utility process', true, {
    electron: described.electron,
    abiVersion: described.abiVersion,
  });
  record(
    'the loader selected the Electron binary',
    Array.isArray(described.addons) &&
      described.addons.some((file) => file.includes(`electron-${described.electron.split('.')[0]}`)),
    { addons: described.addons },
  );

  // 2. A full lifecycle through the utility process.
  const storeA = scratch('a');
  const opened = await worker.request('open', { id: 'a', directory: storeA });
  record('open a store', opened.ok, opened.ok ? opened.result : opened.error);
  const exercised = await worker.request('exercise', { id: 'a' });
  record(
    'upsert, get and vector search',
    exercised.ok &&
      exercised.result.documentCount === 2 &&
      exercised.result.missing === 1 &&
      exercised.result.rankingMatchesIndependentScoring &&
      exercised.result.wideIdRoundTripped,
    exercised.ok ? exercised.result : exercised.error,
  );

  // 3. A second utility process on a *different* store succeeds.
  const second = spawnWorker();
  await second.ready();
  const storeB = scratch('b');
  const openedB = await second.request('open', { id: 'b', directory: storeB });
  record('a second utility process opens a different store', openedB.ok,
    openedB.ok ? openedB.result : openedB.error);

  // 4. A second writer on the *same* store is refused, across processes.
  const contended = await second.request('open', { id: 'contended', directory: storeA });
  record(
    'a second writer on one store is refused across processes',
    !contended.ok && contended.error.code !== null,
    contended.ok ? { unexpectedlyOpened: true } : contended.error,
  );

  // 5. Clean shutdown of the second worker releases its store.
  const closedB = await second.request('close', { id: 'b' });
  record('close releases a store', closedB.ok, closedB.ok ? closedB.result : closedB.error);
  second.child.kill();
  await second.exit;

  // 6. Terminating the holder releases ownership, and the store reopens.
  //    `die` exits the process outright, so it never replies.
  worker.notify('die');
  await worker.exit;
  record('a terminated utility process exits', worker.exited !== null, { code: worker.exited });

  const reopener = spawnWorker();
  await reopener.ready();
  let reopened = { ok: false, error: { message: 'not attempted' } };
  for (let attempt = 0; attempt < 40; attempt += 1) {
    reopened = await reopener.request('open', { id: 'reopen', directory: storeA });
    if (reopened.ok) break;
    // The OS releases the lock as part of tearing the process down, which is
    // not instantaneous; a bounded wait is correct, an unbounded one is not.
    await new Promise((resolve) => setTimeout(resolve, 50));
  }
  record('the store reopens after its holder was terminated', reopened.ok,
    reopened.ok ? reopened.result : reopened.error);

  const reExercised = await reopener.request('exercise', { id: 'reopen' });
  record('the reopened store is usable', reExercised.ok,
    reExercised.ok ? reExercised.result : reExercised.error);
  await reopener.request('close', { id: 'reopen' });
  reopener.child.kill();
  await reopener.exit;

  finish(packaged);
}

function finish(packaged) {
  const failed = results.filter((result) => !result.passed);
  const report = {
    packaged,
    electron: process.versions.electron,
    node: process.versions.node,
    platform: `${process.platform}-${process.arch}`,
    passed: results.length - failed.length,
    failed: failed.length,
    results,
  };
  const text = JSON.stringify(report, null, 2);
  if (REPORT) {
    fs.mkdirSync(path.dirname(REPORT), { recursive: true });
    fs.writeFileSync(REPORT, `${text}\n`);
  }
  console.log(text);
  app.exit(failed.length === 0 ? 0 : 1);
}

app.whenReady().then(() => {
  main().catch((error) => {
    record('fixture completed without an unhandled error', false, { error: String(error) });
    finish(app.isPackaged);
  });
});
