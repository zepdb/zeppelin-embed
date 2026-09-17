/**
 * Packs the package, installs the resulting tarball into a throwaway consumer,
 * and exercises it there.
 *
 * The point is to test what a consumer actually receives. Requiring the module
 * from the source tree would prove nothing about the tarball: it would still
 * pass if `files` omitted the binary, if the loader resolved a path that is not
 * packed, or if the package metadata refused to install on this platform. So
 * the consumer is created outside the repository, depends on the tarball by
 * path, and the smoke fixtures are copied in rather than run in place.
 *
 * The tarball filename comes from `npm pack --json` rather than being
 * reconstructed from the version, so a version bump or a scope change cannot
 * silently make this test install a stale artifact.
 */
import { spawnSync } from 'node:child_process';
import {
  copyFileSync,
  existsSync,
  mkdirSync,
  mkdtempSync,
  readFileSync,
  rmSync,
  writeFileSync,
} from 'node:fs';
import { tmpdir } from 'node:os';
import { dirname, join, resolve } from 'node:path';
import { fileURLToPath } from 'node:url';

const scriptDirectory = dirname(fileURLToPath(import.meta.url));
const packageDirectory = resolve(scriptDirectory, '..');

/**
 * npm's own CLI entry point, run through this Node executable.
 *
 * On Windows npm is a `.cmd` shim, and current Node refuses to `spawnSync` a
 * `.cmd` without a shell. Going through `process.execPath` sidesteps shell
 * resolution entirely and guarantees the npm being driven is the one that owns
 * this Node installation.
 */
function npmCli() {
  const beside = join(dirname(process.execPath), 'node_modules', 'npm', 'bin', 'npm-cli.js');
  if (existsSync(beside)) return beside;
  // Unix layouts put npm under `lib/node_modules`.
  const unix = join(
    dirname(dirname(process.execPath)),
    'lib',
    'node_modules',
    'npm',
    'bin',
    'npm-cli.js',
  );
  if (existsSync(unix)) return unix;
  throw new Error(
    `could not locate npm-cli.js next to ${process.execPath}; ` +
      'the install smoke needs npm from this Node installation',
  );
}

const NPM_CLI = npmCli();

function run(command, args, options = {}) {
  const result = spawnSync(command, args, { stdio: 'inherit', ...options });
  if (result.error) throw result.error;
  if (result.status !== 0) {
    throw new Error(`${command} ${args.join(' ')} exited with status ${result.status}`);
  }
}

function runNpm(args, options = {}) {
  run(process.execPath, [NPM_CLI, ...args], options);
}

function captureNpm(args, options = {}) {
  const result = spawnSync(process.execPath, [NPM_CLI, ...args], {
    encoding: 'utf8',
    ...options,
  });
  if (result.error) throw result.error;
  if (result.status !== 0) {
    throw new Error(`npm ${args.join(' ')} exited with status ${result.status}\n${result.stderr}`);
  }
  return result.stdout;
}

const staging = mkdtempSync(join(tmpdir(), 'zeppelin-node-install-'));
try {
  // 1. Pack, and read the real filename out of npm's own report.
  const packed = captureNpm(['pack', '--json', '--pack-destination', staging], {
    cwd: packageDirectory,
  });
  const report = JSON.parse(packed);
  if (!Array.isArray(report) || report.length !== 1) {
    throw new Error(`npm pack --json reported ${report.length ?? 0} tarballs, expected exactly 1`);
  }
  const tarball = join(staging, report[0].filename);
  console.log(`packed ${report[0].filename} (${report[0].size} bytes, ${report[0].entryCount} entries)`);

  // The tarball must actually contain a binary for this platform, or the
  // install below would succeed and the require would fail confusingly.
  const expectedBinary =
    process.platform === 'win32'
      ? 'prebuilds/win32-x64/node-napi8/zeppelin_embed.node'
      : 'prebuilds/darwin-arm64/zeppelin_embed.node';
  const entries = report[0].files.map((file) => file.path);
  if (!entries.includes(expectedBinary)) {
    throw new Error(
      `the tarball does not contain ${expectedBinary}; it contains:\n  ${entries.join('\n  ')}`,
    );
  }

  // 2. A consumer that lives outside the repository, so nothing can resolve
  //    back into the source tree.
  const consumer = join(staging, 'consumer');
  mkdirSync(consumer, { recursive: true });
  writeFileSync(
    join(consumer, 'package.json'),
    `${JSON.stringify(
      {
        name: 'zeppelin-install-smoke',
        version: '0.0.0',
        private: true,
        type: 'commonjs',
      },
      null,
      2,
    )}\n`,
  );
  runNpm(['install', '--no-audit', '--no-fund', tarball], { cwd: consumer });

  // 3. Run the runtime smoke against the installed copy.
  copyFileSync(
    join(packageDirectory, 'test', 'installed-package-smoke.cjs'),
    join(consumer, 'smoke.cjs'),
  );
  run(process.execPath, ['smoke.cjs'], { cwd: consumer });
  console.log('installed-package runtime smoke: ok');

  // 4. Compile the TypeScript example against the installed type declarations.
  copyFileSync(
    join(packageDirectory, 'test', 'installed-package-type-smoke.ts'),
    join(consumer, 'type-smoke.ts'),
  );
  writeFileSync(
    join(consumer, 'tsconfig.json'),
    `${JSON.stringify(
      {
        compilerOptions: {
          target: 'ES2022',
          module: 'CommonJS',
          moduleResolution: 'node',
          strict: true,
          noEmit: true,
          types: ['node'],
        },
        files: ['type-smoke.ts'],
      },
      null,
      2,
    )}\n`,
  );
  const typescript = JSON.parse(
    readFileSync(join(packageDirectory, 'package.json'), 'utf8'),
  ).devDependencies.typescript;
  runNpm(
    ['install', '--no-audit', '--no-fund', '--save-dev', `typescript@${typescript}`, '@types/node'],
    { cwd: consumer },
  );
  run(process.execPath, [join(consumer, 'node_modules', 'typescript', 'bin', 'tsc'), '--noEmit', '-p', 'tsconfig.json'], {
    cwd: consumer,
  });
  console.log('installed-package TypeScript smoke: ok');

  console.log('INSTALL SMOKE PASSED');
} finally {
  rmSync(staging, { force: true, recursive: true });
}
