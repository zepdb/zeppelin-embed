import { mkdirSync } from 'node:fs';
import { dirname, join, resolve } from 'node:path';
import { spawnSync } from 'node:child_process';
import { fileURLToPath } from 'node:url';

function run(command, args, options = {}) {
  const result = spawnSync(command, args, { stdio: 'inherit', ...options });
  if (result.error) throw result.error;
  if (result.status !== 0) {
    throw new Error(`${command} exited with status ${result.status}`);
  }
}

if (process.platform !== 'darwin' || process.arch !== 'arm64') {
  throw new Error(
    `The initial native package targets macOS arm64; received ${process.platform}/${process.arch}`,
  );
}

const scriptDirectory = dirname(fileURLToPath(import.meta.url));
const packageDirectory = resolve(scriptDirectory, '..');
const repository = resolve(packageDirectory, '..');
const nodePrefix = resolve(dirname(process.execPath), '..');
const nodeHeaders = join(nodePrefix, 'include', 'node');
const ffiHeaders = join(repository, 'crates', 'zeppelin-embed-ffi', 'include');
const source = join(packageDirectory, 'native', 'addon.cc');
const targetDirectory = resolve(process.env.CARGO_TARGET_DIR || join(repository, 'target'));
const staticLibrary = join(targetDirectory, 'release', 'libzeppelin_embed_ffi.a');
const output = join(
  packageDirectory,
  'prebuilds',
  'darwin-arm64',
  'zeppelin_embed.node',
);
const environment = {
  ...process.env,
  MACOSX_DEPLOYMENT_TARGET: process.env.MACOSX_DEPLOYMENT_TARGET || '11.0',
};

run('cargo', ['build', '--locked', '--release', '-p', 'zeppelin-embed-ffi'], {
  cwd: repository,
  env: environment,
});
mkdirSync(dirname(output), { recursive: true });
run('xcrun', [
  'clang++',
  '-std=c++17',
  '-fno-rtti',
  '-O3',
  '-Wall',
  '-Wextra',
  '-Werror',
  '-DNAPI_VERSION=8',
  '-I',
  nodeHeaders,
  '-I',
  ffiHeaders,
  '-bundle',
  '-undefined',
  'dynamic_lookup',
  source,
  staticLibrary,
  '-liconv',
  '-Wl,-dead_strip',
  '-o',
  output,
], { env: environment });
