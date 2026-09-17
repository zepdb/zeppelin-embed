import { copyFileSync, mkdirSync } from 'node:fs';
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

const scriptDirectory = dirname(fileURLToPath(import.meta.url));
const packageDirectory = resolve(scriptDirectory, '..');
const repository = resolve(packageDirectory, '..', '..');
const ffiHeaders = join(repository, 'crates', 'zeppelin-embed-ffi', 'include');
const source = join(packageDirectory, 'native', 'addon.cc');
const targetDirectory = resolve(process.env.CARGO_TARGET_DIR || join(repository, 'target'));

const isDarwinArm64 = process.platform === 'darwin' && process.arch === 'arm64';
const isWindowsX64 = process.platform === 'win32' && process.arch === 'x64';

if (!isDarwinArm64 && !isWindowsX64) {
  throw new Error(
    `The native package targets macOS arm64 and Windows x64; received ${process.platform}/${process.arch}`,
  );
}

if (isDarwinArm64) {
  const nodePrefix = resolve(dirname(process.execPath), '..');
  const nodeHeaders = join(nodePrefix, 'include', 'node');
  const staticLibrary = join(targetDirectory, 'release', 'libzeppelin_embed_ffi.a');
  const output = join(packageDirectory, 'prebuilds', 'darwin-arm64', 'zeppelin_embed.node');
  const environment = {
    ...process.env,
    MACOSX_DEPLOYMENT_TARGET: process.env.MACOSX_DEPLOYMENT_TARGET || '11.0',
  };

  run('cargo', ['build', '--locked', '--release', '-p', 'zeppelin-embed-ffi'], {
    cwd: repository,
    env: environment,
  });
  mkdirSync(dirname(output), { recursive: true });
  run(
    'xcrun',
    [
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
    ],
    { env: environment },
  );
} else {
  // Windows x64.
  //
  // The Rust static library is built first and its absolute path handed to
  // node-gyp, which then compiles `addon.cc` with MSVC and links it in. The
  // addon therefore carries the engine inside it and needs no Zeppelin DLL
  // beside it at run time, which is the same arrangement as the macOS build.
  const target = 'x86_64-pc-windows-msvc';
  run(
    'cargo',
    [
      'build',
      '--locked',
      '--release',
      '--target',
      target,
      '-p',
      'zeppelin-embed-ffi',
      '--no-default-features',
    ],
    { cwd: repository },
  );

  // The static *implementation* archive. Deliberately not
  // `zeppelin_embed_ffi.dll.lib`, which is the DLL's import library.
  const staticLibrary = join(targetDirectory, target, 'release', 'zeppelin_embed_ffi.lib');
  const gypDirectory = join(packageDirectory, 'native', 'windows');
  const nodeGyp = join(packageDirectory, 'node_modules', 'node-gyp', 'bin', 'node-gyp.js');

  // Invoke the build tool through this Node executable. `spawnSync('npm')`
  // and friends do not resolve a Windows `.cmd` shim without a shell, and
  // going through `process.execPath` avoids depending on shell resolution at
  // all.
  const buildVariant = ({ label, output, extraArgs }) => {
    run(
      process.execPath,
      [
        nodeGyp,
        'rebuild',
        `--directory=${gypDirectory}`,
        '--release',
        '--arch=x64',
        ...extraArgs,
        '--',
        `-Dze_ffi_lib=${staticLibrary}`,
        `-Dze_ffi_include=${ffiHeaders}`,
      ],
      { cwd: packageDirectory },
    );

    // Each node-gyp run overwrites `build/Release`, so every variant is copied
    // to its own staging path before the next rebuild starts.
    const built = join(gypDirectory, 'build', 'Release', 'zeppelin_embed.node');
    mkdirSync(dirname(output), { recursive: true });
    copyFileSync(built, output);
    console.log(`built ${label} -> ${output}`);
  };

  const variants = [
    {
      label: 'node-napi8',
      output: join(
        packageDirectory,
        'prebuilds',
        'win32-x64',
        'node-napi8',
        'zeppelin_embed.node',
      ),
      extraArgs: [`--target=${process.versions.node}`],
    },
  ];

  // The Electron variant is built only when asked for, because it downloads
  // Electron's headers. `ZE_ELECTRON_VERSION` selects the version; the loader
  // only accepts majors this package actually ships.
  const electronVersion = process.env.ZE_ELECTRON_VERSION;
  if (electronVersion) {
    const major = Number.parseInt(electronVersion.split('.')[0], 10);
    variants.push({
      label: `electron-${major}`,
      output: join(
        packageDirectory,
        'prebuilds',
        'win32-x64',
        `electron-${major}`,
        'zeppelin_embed.node',
      ),
      extraArgs: [
        `--target=${electronVersion}`,
        '--dist-url=https://electronjs.org/headers',
      ],
    });
  }

  for (const variant of variants) {
    buildVariant(variant);
  }
}
