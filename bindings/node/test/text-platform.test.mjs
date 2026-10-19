import test from 'node:test';
import assert from 'node:assert/strict';
import { readFileSync } from 'node:fs';
import vm from 'node:vm';
import { createRequire } from 'node:module';
const require = createRequire(import.meta.url);
const source = readFileSync(new URL('../index.js', import.meta.url), 'utf8');
// Exercise the real loader with platform metadata and a text-free addon. No
// text engine is mocked: unsupported platforms must reject before native I/O.
for (const platform of ['darwin', 'win32']) {
  test(`${platform}-x64 refuses text with a typed platform error`, async () => {
    const exports = {exports: {}};
    const context = vm.createContext({module: exports, __dirname: '/package',
      process: {platform, arch: 'x64', versions: {napi: '8'}},
      require: name => name.endsWith('.node') ? {textSupported: false} : require(name)});
    vm.runInContext(source, context);
    const {TextStore, ZeppelinError} = exports.exports;
    const unavailable = error => error instanceof ZeppelinError && error.code === 'ZE_ERR_UNSUPPORTED' &&
      error.errorCode === 11 && error.message === 'text API is available on macOS arm64 only';
    assert.throws(() => TextStore.open('unused', 'unused'), unavailable);
    await assert.rejects(TextStore.openAsync('unused', 'unused'), unavailable);
    await assert.rejects(TextStore.openModel({}), unavailable);
  });
}
