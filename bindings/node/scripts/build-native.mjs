import { copyFileSync, mkdirSync, readFileSync, writeFileSync } from 'node:fs';
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
const exportsFile = join(packageDirectory, 'native', 'exports.txt');
const packageNative = join(repository, 'scripts', 'release', 'package-native.py');
const targetDirectory = resolve(process.env.CARGO_TARGET_DIR || join(repository, 'target'));

const isDarwin = process.platform === 'darwin';
const isWindowsX64 = process.platform === 'win32' && process.arch === 'x64';
const python = process.env.PYTHON || (isDarwin ? 'python3' : 'python');

// Keep Cargo outputs/rlibs intact; only these distribution copies lose bitcode.
function distributionArchive(input, target) {
  const output = join(targetDirectory, 'native-archives', target,
    isDarwin ? 'libzeppelin_embed_ffi.a' : 'zeppelin_embed_ffi.lib');
  run(python, [packageNative, 'archive', input, output]);
  return output;
}

if (!isDarwin && !isWindowsX64) {
  throw new Error(
    `The native package targets macOS and Windows x64; received ${process.platform}/${process.arch}`,
  );
}

/**
 * Every prebuild carries graph: both macOS slices and Windows Node/Electron.
 * Both macOS architectures are built on either kind of Mac.
 *
 * The addon is a `-bundle` with `-undefined dynamic_lookup`, so it links
 * against no Node library and its Node-API symbols resolve from whatever host
 * process loads it. That makes it a pure cross-compile: one slice per Rust
 * target plus the matching `clang -arch`, with architecture-independent Node
 * headers. It is also why macOS needs no per-Electron-major variant the way
 * Windows does, where the addon delay-loads `node.exe` and the import library
 * differs per host.
 *
 * Both slices are always built, rather than only the host's. A Mac that can
 * produce one can produce the other, and building both means a local
 * `npm pack` yields a complete macOS package instead of one that advertises
 * an architecture it silently omits.
 */
const MACOS_SLICES = [
  { directory: 'darwin-arm64', rustTarget: 'aarch64-apple-darwin', clangArch: 'arm64' },
  { directory: 'darwin-x64', rustTarget: 'x86_64-apple-darwin', clangArch: 'x86_64' },
];

if (isDarwin) {
  const nodePrefix = resolve(dirname(process.execPath), '..');
  const nodeHeaders = join(nodePrefix, 'include', 'node');
  for (const slice of MACOS_SLICES) {
    // MLX's Metal kernels need Metal 3.0, available from macOS 13.0; ZE-522
    // proved MLX builds and runs there (bfloat kernels are omitted).
    const environment = {...process.env, MACOSX_DEPLOYMENT_TARGET:
      process.env.MACOSX_DEPLOYMENT_TARGET || (slice.clangArch === 'arm64' ? '13.0' : '11.0')};
    const staticLibrary = join(
      targetDirectory,
      slice.rustTarget,
      'release',
      'libzeppelin_embed_ffi.a',
    );
    const output = join(packageDirectory, 'prebuilds', slice.directory, 'zeppelin_embed.node');

    // `--target` is passed even for the host, so both slices land under
    // `target/<triple>/release` and neither can pick up the other's artifacts
    // from the bare `target/release` directory.
    const built = spawnSync('cargo',
      ['build', '--locked', '--release', '--target', slice.rustTarget, '-p', 'zeppelin-embed-ffi',
        '--features', slice.clangArch === 'arm64' ? 'graph-cypher,text' : 'graph-cypher',
        '--message-format=json-render-diagnostics'],
      {cwd: repository, env: environment, encoding: 'utf8', stdio: ['ignore', 'pipe', 'inherit'], maxBuffer: 64 * 1024 * 1024});
    if (built.error) throw built.error;
    if (built.status !== 0) throw new Error(`cargo build exited with status ${built.status}`);
    if (slice.clangArch === 'arm64') {
      // Cargo reports the exact mlx-sys build output even on a cached build.
      const mlx = built.stdout.trim().split('\n').map(line => JSON.parse(line))
        .filter(event => event.reason === 'build-script-executed' && event.package_id.includes('mlx-sys@'));
      if (mlx.length !== 1) throw new Error(`expected one mlx-sys build output, got ${mlx.length}`);
      mkdirSync(dirname(output), {recursive: true});
      copyFileSync(join(mlx[0].out_dir, 'build', 'lib', 'mlx.metallib'), join(dirname(output), 'mlx.metallib'));
    }
    const archive = distributionArchive(staticLibrary, slice.rustTarget);
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
        '-arch',
        slice.clangArch,
        '-DNAPI_VERSION=8',
        '-DZE_GRAPH',
        ...(slice.clangArch === 'arm64' ? ['-DZE_TEXT'] : []),
        '-I',
        nodeHeaders,
        '-I',
        ffiHeaders,
        '-bundle',
        '-undefined',
        'dynamic_lookup',
        source,
        archive,
        '-liconv',
        ...(slice.clangArch === 'arm64' ? ['-lobjc', '-framework', 'Foundation', '-framework', 'CoreML', '-framework', 'Metal', '-framework', 'Accelerate'] : []),
        '-fvisibility=hidden',
        `-Wl,-exported_symbols_list,${exportsFile}`,
        '-Wl,-dead_strip',
        '-o',
        output,
      ],
      { env: environment },
    );
    run('xcrun', ['strip', '-S', '-x', output]);
    console.log(`built ${slice.directory} -> ${output}`);
  }
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
      '--features',
      'graph-cypher',
    ],
    { cwd: repository },
  );

  // The static *implementation* archive. Deliberately not
  // `zeppelin_embed_ffi.dll.lib`, which is the DLL's import library.
  const staticLibrary = join(targetDirectory, target, 'release', 'zeppelin_embed_ffi.lib');
  const archive = distributionArchive(staticLibrary, target);
  const exportsDefinition = join(targetDirectory, 'native-archives', target, 'node.def');
  const exports = readFileSync(exportsFile, 'utf8').trim().split(/\r?\n/).map(name => name.slice(1));
  writeFileSync(exportsDefinition, `EXPORTS\n${exports.join('\n')}\n`);
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
        `-Dze_ffi_lib=${archive}`,
        `-Dze_exports_def=${exportsDefinition}`,
        '-Dze_link_options=/OPT:REF /OPT:ICF /INCREMENTAL:NO /DEBUG:NONE',
        `-Dze_ffi_include=${ffiHeaders}`,
      ],
      // CL supplies the graph define to both MSVC variants without changing gyp.
      { cwd: packageDirectory, env: { ...process.env, CL: `${process.env.CL || ''} /DZE_GRAPH` } },
    );

    // Each node-gyp run overwrites `build/Release`, so every variant is copied
    // to its own staging path before the next rebuild starts.
    const built = join(gypDirectory, 'build', 'Release', 'zeppelin_embed.node');
    mkdirSync(dirname(output), { recursive: true });
    copyFileSync(built, output);
    run(python, [packageNative, 'windows-addon', output, output]);
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
